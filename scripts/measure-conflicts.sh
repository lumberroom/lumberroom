#!/usr/bin/env bash
# Gates E1-G3 and E2-G2 of docs/specs/write-time-conflicts.md, on a restored copy of a real store.
#
#   DATABASE_URL=postgres://... scripts/measure-conflicts.sh --scratch [options]
#
#   --scratch            required: this script writes conflict pairs and scan marks
#   --tenant T           tenant to measure (default: the tenant with the most live rows)
#   --floor F            CONFLICT_THRESHOLD the store runs at (default 0.90)
#   --runs N             timed runs per statement (default 20)
#   --limit N            page size for the conflicts read (default 50, the queue's page)
#   --grant GLOB=MAX     a reader grant, repeatable (default '*=sealed', the owner)
#   --role R             SET ROLE before every session; use the serving role for the reads
#   --sweep              drain the backlog first, one committed scan per row, timing each
#   --out DIR            where the report and plans go (default ./conflicts-measure)
#
# PSQL overrides the client, so a box with Postgres only inside a container works:
#   PSQL="docker exec -i lumberroom-db-1 psql" DATABASE_URL=postgres://u:p@localhost/scratch ...
#
# Run it against a scratch database restored from a dump, never the serving store: the sweep and
# the E1-G3 timing call memory_conflict_record, which inserts pairs and marks. The SQL for the read
# below is a copy of CONFLICTS_SQL in src/adapters/postgres/memory.rs, which spec section 4.9
# fixes; change the spec, then the constant, then this copy.
set -euo pipefail

PSQL="${PSQL:-psql}"
TENANT=""
FLOOR="0.90"
RUNS=20
LIMIT=50
ROLE=""
SWEEP=0
SCRATCH=0
OUT="./conflicts-measure"
GRANTS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --scratch) SCRATCH=1 ;;
    --tenant) TENANT="$2"; shift ;;
    --floor) FLOOR="$2"; shift ;;
    --runs) RUNS="$2"; shift ;;
    --limit) LIMIT="$2"; shift ;;
    --grant) GRANTS+=("$2"); shift ;;
    --role) ROLE="$2"; shift ;;
    --sweep) SWEEP=1 ;;
    --out) OUT="$2"; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "measure-conflicts: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done

if [ "$SCRATCH" != 1 ]; then
  echo "measure-conflicts: pass --scratch to confirm DATABASE_URL is a restored copy." >&2
  echo "  The sweep and the E1-G3 timing write pairs and marks into it." >&2
  exit 2
fi
: "${DATABASE_URL:?set DATABASE_URL to the scratch copy}"
[ ${#GRANTS[@]} -eq 0 ] && GRANTS=("*=sealed")

# Grants become the three arrays grant_arrays() binds: a trailing * is a prefix match, anything
# else an exact namespace.
g_prefix=""; g_exact=""; g_max=""
for g in "${GRANTS[@]}"; do
  glob="${g%=*}"; max="${g##*=}"
  case "$max" in open|private|sealed) ;; *) echo "measure-conflicts: bad ceiling in $g" >&2; exit 2 ;; esac
  if [ "${glob%\*}" != "$glob" ]; then p="${glob%\*}"; e=false; else p="$glob"; e=true; fi
  g_prefix+="${g_prefix:+,}\"${p//\"/}\""; g_exact+="${g_exact:+,}$e"; g_max+="${g_max:+,}$max"
done
G_PREFIX="{$g_prefix}"; G_EXACT="{$g_exact}"; G_MAX="{$g_max}"

mkdir -p "$OUT"

# One psql session per call. Every session sets the role and the measure.* settings, which the DO
# blocks read because psql does not interpolate variables inside dollar quotes.
q() {
  {
    [ -n "$ROLE" ] && printf 'SET ROLE %s;\n' "$ROLE"
    printf "SET measure.tenant = %s;\nSET measure.floor = '%s';\n" "$(sql_lit "$TENANT")" "$FLOOR"
    cat
  } | $PSQL "$DATABASE_URL" -X -q -At -v ON_ERROR_STOP=1
}
sql_lit() { printf "'%s'" "${1//\'/\'\'}"; }

p50() { sort -n | awk '{a[NR]=$1} END {if (NR==0) {print "-"; exit} i=int((NR+1)/2); print a[i]}'; }
p95() { sort -n | awk '{a[NR]=$1} END {if (NR==0) {print "-"; exit} i=int(NR*0.95+0.999); if (i>NR) i=NR; print a[i]}'; }
maxv() { sort -n | tail -1; }

echo "== preflight"
TENANT_ARG="$TENANT"
TENANT="${TENANT:-x}"
migrated=$(q <<'SQL'
SELECT count(*) FROM _sqlx_migrations WHERE version = 20261004000026 AND success;
SQL
)
if [ "$migrated" != 1 ]; then
  echo "measure-conflicts: migration 20261004000026 (conflict pairs) is not applied here." >&2
  exit 1
fi
if [ -z "$TENANT_ARG" ]; then
  TENANT=$(q <<'SQL'
SELECT tenant_id FROM memory
 WHERE superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
 GROUP BY tenant_id ORDER BY count(*) DESC LIMIT 1;
SQL
)
fi
[ -n "$TENANT" ] || { echo "measure-conflicts: no live rows in this database" >&2; exit 1; }

IFS='|' read -r PG_VERSION LIVE EMBEDDED NAMESPACES BIG_NS BIG_ROWS < <(q <<'SQL'
WITH live AS (
  SELECT namespace, embedding IS NOT NULL AS embedded FROM memory
   WHERE tenant_id = current_setting('measure.tenant')
     AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
), big AS (
  SELECT namespace, count(*) AS n FROM live GROUP BY namespace ORDER BY n DESC, namespace LIMIT 1
)
SELECT current_setting('server_version'),
       (SELECT count(*) FROM live),
       (SELECT count(*) FROM live WHERE embedded),
       (SELECT count(DISTINCT namespace) FROM live),
       (SELECT namespace FROM big), (SELECT n FROM big);
SQL
)
backlog() {
  q <<'SQL'
SELECT memory_conflict_backlog(current_setting('measure.tenant'), current_setting('measure.floor')::float8);
SQL
}
BACKLOG_BEFORE=$(backlog)
echo "tenant $TENANT: $LIVE live rows ($EMBEDDED embedded) in $NAMESPACES namespaces;" \
     "largest $BIG_NS with $BIG_ROWS; backlog $BACKLOG_BEFORE"

SWEPT=0; SWEEP_P50="-"; SWEEP_P95="-"; SWEEP_MAX="-"; SWEEP_BIG_P50="-"; SWEEP_SECS="-"
if [ "$SWEEP" = 1 ] && [ "$BACKLOG_BEFORE" -gt 0 ]; then
  echo "== sweep: one committed memory_conflict_record per row, oldest first"
  start=$(date +%s)
  # COMMIT inside DO needs the block outside a transaction, which psql's autocommit gives it. One
  # scan per transaction mirrors the sweeper (spec section 6); a batch transaction would hold the
  # tenant's shared key and every pair key until the end.
  q > "$OUT/sweep.tsv" <<'SQL'
CREATE TEMP TABLE sweep_t (id uuid, namespace text, ms float8);
DO $$
DECLARE
  t text := current_setting('measure.tenant');
  f float8 := current_setting('measure.floor')::float8;
  r uuid; t0 timestamptz; stuck int := 0; done int;
BEGIN
  LOOP
    done := 0;
    FOR r IN SELECT id FROM memory_conflict_next(t, f, 200) AS id LOOP
      t0 := clock_timestamp();
      BEGIN
        PERFORM memory_conflict_record(t, r, f);
        done := done + 1;
      EXCEPTION WHEN lock_not_available THEN
        RAISE WARNING 'scan of % hit the lock timeout; it stays pending', r;
      END;
      INSERT INTO sweep_t SELECT r, m.namespace,
             extract(epoch FROM clock_timestamp() - t0) * 1000 FROM memory m WHERE m.id = r;
      COMMIT;
    END LOOP;
    EXIT WHEN done = 0;
  END LOOP;
END $$;
SELECT namespace, round(ms::numeric, 3) FROM sweep_t;
SQL
  SWEEP_SECS=$(( $(date +%s) - start ))
  SWEPT=$(wc -l < "$OUT/sweep.tsv")
  SWEEP_P50=$(cut -d'|' -f2 "$OUT/sweep.tsv" | p50)
  SWEEP_P95=$(cut -d'|' -f2 "$OUT/sweep.tsv" | p95)
  SWEEP_MAX=$(cut -d'|' -f2 "$OUT/sweep.tsv" | maxv)
  SWEEP_BIG_P50=$(awk -F'|' -v ns="$BIG_NS" '$1==ns {print $2}' "$OUT/sweep.tsv" | p50)
  echo "swept $SWEPT rows in ${SWEEP_SECS}s: p50 ${SWEEP_P50} ms, p95 ${SWEEP_P95} ms, max ${SWEEP_MAX} ms"
fi
BACKLOG_AFTER=$(backlog)
echo "backlog now $BACKLOG_AFTER"
[ "$BACKLOG_AFTER" -gt 0 ] && echo "  WARNING: rows still pending; the E2-G2 read below misses their pairs." >&2

# Pull the timing out of a text EXPLAIN ANALYZE stream, one number per plan.
exec_ms() { awk '/Execution Time:/ {print $3}'; }

echo "== E1-G3: memory_conflict_record on the largest namespace ($BIG_NS, $BIG_ROWS rows)"
ANCHOR=$(q <<SQL
SELECT id FROM memory
 WHERE tenant_id = current_setting('measure.tenant') AND namespace = $(sql_lit "$BIG_NS")
   AND embedding IS NOT NULL AND superseded_by IS NULL
   AND (occurred_until IS NULL OR occurred_until > now())
 ORDER BY created_at DESC, id DESC LIMIT 1;
SQL
)
# EXPLAIN on a function call shows one Result node, so the plan comes from the function's own
# SELECT, copied from migration 20261004000026 without the INSERT around it. The binds go in as
# literals, which is what plpgsql's first custom plans see.
q > "$OUT/e1-g3-scan-plan.txt" <<SQL
EXPLAIN (ANALYZE, BUFFERS)
WITH anchor AS MATERIALIZED (
  SELECT id, namespace, created_at, embedding FROM memory
   WHERE tenant_id = $(sql_lit "$TENANT") AND id = '$ANCHOR' AND embedding IS NOT NULL
     AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
), near AS MATERIALIZED (
  SELECT m.id, m.created_at, a.id AS anchor_id, a.created_at AS anchor_created_at,
         (1 - (m.embedding <=> a.embedding))::float8 AS similarity
    FROM anchor a
    JOIN memory m
      ON m.tenant_id = $(sql_lit "$TENANT") AND m.namespace = a.namespace AND m.id <> a.id
   WHERE m.superseded_by IS NULL
     AND (m.occurred_until IS NULL OR m.occurred_until > now())
     AND m.embedding IS NOT NULL
)
SELECT count(*) FROM near WHERE similarity >= $FLOOR::float8;
SQL
q > "$OUT/e1-g3-record.tsv" <<SQL
CREATE TEMP TABLE rec_t (ms float8);
DO \$\$
DECLARE t0 timestamptz;
BEGIN
  FOR i IN 1..$RUNS LOOP
    t0 := clock_timestamp();
    PERFORM memory_conflict_record(current_setting('measure.tenant'), '$ANCHOR',
                                   current_setting('measure.floor')::float8);
    INSERT INTO rec_t VALUES (extract(epoch FROM clock_timestamp() - t0) * 1000);
    COMMIT;
  END LOOP;
END \$\$;
SELECT round(ms::numeric, 3) FROM rec_t;
SQL
REC_P50=$(p50 < "$OUT/e1-g3-record.tsv"); REC_P95=$(p95 < "$OUT/e1-g3-record.tsv")

explain_runs() { # $1 name, stdin: the statement to EXPLAIN, once per run
  local stmt; stmt=$(cat)
  for _ in $(seq "$RUNS"); do printf '\\echo ---PLAN---\nEXPLAIN (ANALYZE, BUFFERS) %s;\n' "$stmt"; done \
    | q > "$OUT/$1-plans.txt"
  exec_ms < "$OUT/$1-plans.txt" > "$OUT/$1.ms"
}
explain_runs e1-g3-next <<'SQL'
SELECT * FROM memory_conflict_next(current_setting('measure.tenant'), current_setting('measure.floor')::float8, 50)
SQL
explain_runs e1-g3-backlog <<'SQL'
SELECT memory_conflict_backlog(current_setting('measure.tenant'), current_setting('measure.floor')::float8)
SQL

scan_plan="$OUT/e1-g3-scan-plan.txt"
SCAN_MS=$(exec_ms < "$scan_plan")
SCAN_SORT=$(grep -c ' Sort ' "$scan_plan" || true)
SCAN_HNSW=$(grep -ci 'hnsw' "$scan_plan" || true)
SCAN_INDEX=$(grep -oE '(Index (Only )?Scan using [a-z0-9_]+|Bitmap Index Scan on [a-z0-9_]+|Seq Scan on [a-z0-9_]+)' "$scan_plan" | sort -u | paste -sd ';' - || true)
echo "scan plan ${SCAN_MS} ms (Sort nodes: $SCAN_SORT, HNSW: $SCAN_HNSW, access: ${SCAN_INDEX:-none})"
echo "record p50 $REC_P50 ms, p95 $REC_P95 ms over $RUNS committed calls"

echo "== E2-G2: the conflicts read and the pending count, $RUNS runs each, one prepared statement"
# A prepared statement like the server's: sqlx prepares, and after five executions Postgres may
# switch to a generic plan, so run 1 and run $RUNS can differ. Both plans are kept.
read_session() { # $1 name, $2 PREPARE text, $3 EXECUTE args
  { printf '%s\n' "$2"
    for _ in $(seq "$RUNS"); do printf '\\echo ---PLAN---\nEXPLAIN (ANALYZE, BUFFERS) EXECUTE s(%s);\n' "$3"; done
  } | q > "$OUT/$1-plans.txt"
  exec_ms < "$OUT/$1-plans.txt" > "$OUT/$1.ms"
}
BINDS_GRANT="$(sql_lit "$G_PREFIX")::text[], $(sql_lit "$G_EXACT")::bool[], $(sql_lit "$G_MAX")::text[]"
read_session e2-g2-conflicts "PREPARE s(text, float8, int8, int8, text[], bool[], text[]) AS
SELECT a.id AS older_id, a.namespace AS older_namespace,
       COALESCE(a.content, '') AS older_content,
       b.id AS newer_id, b.namespace AS newer_namespace,
       COALESCE(b.content, '') AS newer_content,
       c.similarity
  FROM memory_conflict c
  JOIN memory a ON a.tenant_id = c.tenant_id AND a.id = c.older_id
  JOIN memory b ON b.tenant_id = c.tenant_id AND b.id = c.newer_id
 WHERE c.tenant_id = \$1
   AND c.similarity >= \$2
   AND a.namespace = b.namespace
   AND a.superseded_by IS NULL
   AND (a.occurred_until IS NULL OR a.occurred_until > now())
   AND b.superseded_by IS NULL
   AND (b.occurred_until IS NULL OR b.occurred_until > now())
   AND EXISTS (
         SELECT 1
           FROM unnest(\$5::text[], \$6::bool[], \$7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN a.namespace = g.prefix
                     ELSE left(a.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(a.sensitivity)
       )
   AND EXISTS (
         SELECT 1
           FROM unnest(\$5::text[], \$6::bool[], \$7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN b.namespace = g.prefix
                     ELSE left(b.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(b.sensitivity)
       )
   AND NOT EXISTS (
         SELECT 1 FROM memory_pair_dismissed d
          WHERE d.tenant_id = c.tenant_id
            AND d.lo_id = least(a.id, b.id)
            AND d.hi_id = greatest(a.id, b.id)
       )
 ORDER BY c.similarity DESC, a.created_at, a.id, b.id
 LIMIT \$3 OFFSET \$4;" \
  "$(sql_lit "$TENANT"), $FLOOR, $LIMIT, 0, $BINDS_GRANT"
read_session e2-g2-pending "PREPARE s(text, float8, text[], bool[], text[]) AS
SELECT count(*) FROM memory m
 WHERE m.tenant_id = \$1
   AND m.superseded_by IS NULL AND (m.occurred_until IS NULL OR m.occurred_until > now())
   AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                    WHERE s.memory_id = m.id AND s.floor <= \$2)
   AND EXISTS (
         SELECT 1
           FROM unnest(\$3::text[], \$4::bool[], \$5::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                     ELSE left(m.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(m.sensitivity)
       );" \
  "$(sql_lit "$TENANT"), $FLOOR, $BINDS_GRANT"

PAIRS=$(q <<'SQL'
SELECT count(*) FROM memory_conflict
 WHERE tenant_id = current_setting('measure.tenant')
   AND similarity >= current_setting('measure.floor')::float8;
SQL
)
row() { printf '| %s | %s | %s | %s |\n' "$1" "$(p50 < "$OUT/$2.ms")" "$(p95 < "$OUT/$2.ms")" "$(maxv < "$OUT/$2.ms")"; }

# The plan from the first run and the last, the second being the one a long-lived pool settles on.
first_last_plan() {
  awk '/^---PLAN---$/ {n++; next} {p[n] = p[n] $0 "\n"} END {printf "-- run 1\n%s\n-- run %d\n%s", p[1], n, p[n]}' "$OUT/$1-plans.txt"
}

{
  echo "# Write-time conflicts: E1-G3 and E2-G2 measurements"
  echo
  echo "Taken $(date -u +%Y-%m-%dT%H:%MZ) on Postgres $PG_VERSION by scripts/measure-conflicts.sh."
  echo "Tenant \`$TENANT\`: $LIVE live rows ($EMBEDDED with a vector) in $NAMESPACES namespaces."
  echo "Largest namespace \`$BIG_NS\`, $BIG_ROWS live rows. Floor $FLOOR. Grants: ${GRANTS[*]}."
  echo "Role: ${ROLE:-the connecting user}. $RUNS runs per statement."
  echo
  echo "## Backfill"
  echo
  echo "| Backlog before | Rows swept | Wall time | p50 per row | p95 per row | max | p50 in largest namespace | Backlog after |"
  echo "|---|---|---|---|---|---|---|---|"
  echo "| $BACKLOG_BEFORE | $SWEPT | ${SWEEP_SECS} s | $SWEEP_P50 ms | $SWEEP_P95 ms | $SWEEP_MAX ms | $SWEEP_BIG_P50 ms | $BACKLOG_AFTER |"
  echo
  echo "Stored pairs at or above the floor: $PAIRS."
  echo
  echo "## E1-G3: the scan"
  echo
  echo "Anchor \`$ANCHOR\`, the newest live row in \`$BIG_NS\`."
  echo
  echo "| Statement | p50 ms | p95 ms | max ms |"
  echo "|---|---|---|---|"
  printf '| memory_conflict_record, committed call | %s | %s | %s |\n' "$REC_P50" "$REC_P95" "$(maxv < "$OUT/e1-g3-record.tsv")"
  row "memory_conflict_next(50)" e1-g3-next
  row "memory_conflict_backlog" e1-g3-backlog
  echo
  echo "Scan plan: ${SCAN_MS} ms in EXPLAIN ANALYZE; Sort nodes $SCAN_SORT; HNSW mentions $SCAN_HNSW;"
  echo "access paths: ${SCAN_INDEX:-none}."
  echo
  echo '```'
  cat "$scan_plan"
  echo '```'
  echo
  echo "## E2-G2: the read"
  echo
  echo "| Statement | p50 ms | p95 ms | max ms |"
  echo "|---|---|---|---|"
  row "CONFLICTS_SQL, limit $LIMIT offset 0" e2-g2-conflicts
  row "CONFLICTS_PENDING_SQL" e2-g2-pending
  echo
  echo "### CONFLICTS_SQL plans"
  echo
  echo '```'
  first_last_plan e2-g2-conflicts
  echo '```'
  echo
  echo "### CONFLICTS_PENDING_SQL plans"
  echo
  echo '```'
  first_last_plan e2-g2-pending
  echo '```'
} > "$OUT/report.md"

echo "conflicts read p50 $(p50 < "$OUT/e2-g2-conflicts.ms") ms, pending p50 $(p50 < "$OUT/e2-g2-pending.ms") ms"
echo "report: $OUT/report.md"
