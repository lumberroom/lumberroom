-- Maps each cosine threshold from the model in slot A to the model in slot B by matching the share
-- of neighbour scores at or above it (engine decision 0027). Read only. Run as the table owner
-- against a copy whose two slots are full, never against a serving database:
--   psql -v tenant='*' -v thresholds='dedupe=0.97,conflict=0.90' -f scripts/embedding-thresholds.sql
-- tenant '*' pools every tenant; neighbours always stay within the anchor's tenant and namespace.
-- Swap embedding and embedding_b in the two score CTEs when slot B holds the old model.
-- support under 30 (a design target) means few old scores sit near that threshold.
BEGIN READ ONLY;
-- Exact scans: the HNSW index answers an approximate top 20 and would bend the distribution.
SET LOCAL enable_indexscan = off;
SET LOCAL enable_indexonlyscan = off;
WITH anchor AS (
  SELECT id, tenant_id, namespace, embedding AS va, embedding_b AS vb
    FROM memory
   WHERE (:'tenant' = '*' OR tenant_id = :'tenant')
     AND superseded_by IS NULL AND embedding IS NOT NULL AND embedding_b IS NOT NULL
   ORDER BY md5(id::text)
   LIMIT 2000
), old_scores AS (
  SELECT n.s FROM anchor a CROSS JOIN LATERAL (
    SELECT 1 - (m.embedding <=> a.va) AS s
      FROM memory m
     WHERE m.tenant_id = a.tenant_id AND m.namespace = a.namespace AND m.id <> a.id
       AND m.superseded_by IS NULL AND m.embedding IS NOT NULL
     ORDER BY m.embedding <=> a.va
     LIMIT 20) n
), new_scores AS (
  SELECT n.s FROM anchor a CROSS JOIN LATERAL (
    SELECT 1 - (m.embedding_b <=> a.vb) AS s
      FROM memory m
     WHERE m.tenant_id = a.tenant_id AND m.namespace = a.namespace AND m.id <> a.id
       AND m.superseded_by IS NULL AND m.embedding_b IS NOT NULL
     ORDER BY m.embedding_b <=> a.vb
     LIMIT 20) n
), threshold AS (
  SELECT split_part(kv, '=', 1) AS key, split_part(kv, '=', 2)::float8 AS t_old
    FROM string_to_table(:'thresholds', ',') AS kv
), pool AS (
  SELECT (SELECT count(*) FROM old_scores) AS n_old, (SELECT count(*) FROM new_scores) AS n_new
), mapped AS (
  SELECT t.key, t.t_old, o.support,
         (SELECT s FROM new_scores ORDER BY s DESC
           OFFSET greatest(1, ceil(o.support::float8 * p.n_new / nullif(p.n_old, 0)))::int - 1
           LIMIT 1) AS t_new
    FROM threshold t, pool p,
         LATERAL (SELECT count(*) AS support FROM old_scores WHERE s >= t.t_old) o
)
-- The two trailing rows print the line to paste. Their NULLs and the text column take the first
-- branch's types (text, float8, bigint, numeric), so the casts below only pin what Postgres would
-- infer anyway.
SELECT key, t_old, support, round(t_new::numeric, 3) AS t_new FROM mapped
UNION ALL
SELECT 'EMBED_THRESHOLDS'::text, NULL::float8, NULL::bigint, NULL::numeric
UNION ALL
SELECT string_agg(key || '=' || round(t_new::numeric, 3), ',' ORDER BY key),
       NULL::float8, NULL::bigint, NULL::numeric
  FROM mapped;
ROLLBACK;
