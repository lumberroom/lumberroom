-- Conflict pairs recorded after a row commits, read from a table instead of a self-join per read.
-- docs/specs/write-time-conflicts.md sections 4.1 to 4.5 is the contract, and everything below is
-- pasted from it. Decision 0022 carries the reasoning.
--
-- This file keeps its name for good: downstream manifests list it once it merges. A change to any
-- function here ships as a forward migration with CREATE OR REPLACE, and a deployment that replaced
-- one with a SECURITY DEFINER version must re-apply its replacement after that migration, because
-- CREATE OR REPLACE resets SECURITY DEFINER and SET search_path to the new definition's.

-- One row per live pair at or above the floor it was scanned at. Older and newer by
-- (created_at, id), the order CONFLICTS_SQL reports them in. created_at never changes in place, so
-- the order a scan writes stays true for the life of both rows.
CREATE TABLE IF NOT EXISTS memory_conflict (
  tenant_id  text             NOT NULL,
  older_id   uuid             NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  newer_id   uuid             NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  similarity double precision NOT NULL,
  PRIMARY KEY (older_id, newer_id),
  CHECK (older_id <> newer_id)
);
-- The read: one tenant, most similar first.
CREATE INDEX IF NOT EXISTS memory_conflict_by_similarity
  ON memory_conflict (tenant_id, similarity DESC);
-- The cascade and the invalidation trigger probe newer_id alone; the primary key serves older_id.
CREATE INDEX IF NOT EXISTS memory_conflict_newer ON memory_conflict (newer_id);

-- One row per memory whose pairs are on record, and the floor they were found at. A live row with
-- no mark at or below the configured threshold is pending: the sweeper scans it, and readers count
-- it.
CREATE TABLE IF NOT EXISTS memory_conflict_scan (
  memory_id  uuid             PRIMARY KEY REFERENCES memory(id) ON DELETE CASCADE,
  tenant_id  text             NOT NULL,
  floor      double precision NOT NULL,
  scanned_at timestamptz      NOT NULL DEFAULT now()
);

-- Every live pair p_id forms in its own namespace at or above p_floor, then the mark. Returns
-- nothing: a count of pairs that includes rows the caller cannot read is itself a disclosure.
--
-- No ORDER BY and no LIMIT: this must find every pair above the floor, which a top-k index scan
-- cannot promise. `near` is MATERIALIZED so each candidate's vector is detoasted once:
-- referenced in both the select list and the WHERE clause, it would be unpacked twice.
--
-- TRAP: only the sweeper calls this, as a statement of its own, after the anchor row committed.
-- Never call it from a trigger on memory or inside a transaction that inserted the anchor. Section
-- 6 of docs/specs/write-time-conflicts.md is the reason.
--
-- The shared advisory lock comes first and holds to commit. memory_conflict_forget_row and
-- memory_conflict_rescan_row take the same key exclusive, so a trigger that clears a row's pairs
-- waits for every scan in flight, and a scan that started on an old vector commits before the clear
-- rather than after it.
--
-- lock_timeout bounds every wait in here, the advisory lock and the row locks the pair insert's
-- foreign-key checks take on both memory rows. A transaction that holds a memory row FOR UPDATE and
-- then revives a row waits for this scan's shared key while this scan's FK check waits for its row
-- lock; the timeout ends the scan, the other transaction commits, and the anchor stays unmarked for
-- the next sweep.
CREATE OR REPLACE FUNCTION memory_conflict_record(
    p_tenant text, p_id uuid, p_floor double precision)
RETURNS void LANGUAGE plpgsql SET lock_timeout = '200ms' AS $$
BEGIN
  PERFORM pg_advisory_xact_lock_shared(hashtextextended('memory_conflict:' || p_tenant, 0));
  WITH anchor AS MATERIALIZED (
    SELECT id, namespace, created_at, embedding
      FROM memory
     WHERE tenant_id = p_tenant AND id = p_id AND embedding IS NOT NULL
       AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
  ), near AS MATERIALIZED (
    SELECT m.id, m.created_at, a.id AS anchor_id, a.created_at AS anchor_created_at,
           (1 - (m.embedding <=> a.embedding))::float8 AS similarity
      FROM anchor a
      JOIN memory m
        ON m.tenant_id = p_tenant AND m.namespace = a.namespace AND m.id <> a.id
     WHERE m.superseded_by IS NULL
       AND (m.occurred_until IS NULL OR m.occurred_until > now())
       AND m.embedding IS NOT NULL
  )
  INSERT INTO memory_conflict (tenant_id, older_id, newer_id, similarity)
  SELECT p_tenant,
           CASE WHEN (n.created_at, n.id) < (n.anchor_created_at, n.anchor_id)
                THEN n.id ELSE n.anchor_id END,
           CASE WHEN (n.created_at, n.id) < (n.anchor_created_at, n.anchor_id)
                THEN n.anchor_id ELSE n.id END,
           n.similarity
    FROM near n
   WHERE n.similarity >= p_floor
  ON CONFLICT (older_id, newer_id) DO UPDATE SET similarity = EXCLUDED.similarity;

  -- Marked whether or not it was live or embedded: a mark means "scanned", and a row that becomes
  -- live again loses its mark through the trigger in 4.4.
  INSERT INTO memory_conflict_scan (memory_id, tenant_id, floor)
  SELECT id, tenant_id, p_floor FROM memory WHERE tenant_id = p_tenant AND id = p_id
  ON CONFLICT (memory_id) DO UPDATE SET floor = EXCLUDED.floor, scanned_at = now();
END
$$;

-- Up to p_limit live rows with no current mark, oldest first. The caller records each one in its
-- own statement, so each scan commits on its own (see below).
CREATE OR REPLACE FUNCTION memory_conflict_next(
    p_tenant text, p_floor double precision, p_limit integer)
RETURNS SETOF uuid LANGUAGE sql STABLE AS $$
  SELECT m.id FROM memory m
   WHERE m.tenant_id = p_tenant
     AND m.superseded_by IS NULL
     AND (m.occurred_until IS NULL OR m.occurred_until > now())
     AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                      WHERE s.memory_id = m.id AND s.floor <= p_floor)
   ORDER BY m.created_at, m.id
   LIMIT p_limit
$$;

-- Every live row in the tenant with no current mark, whoever may read it. For background passes
-- only: section 4.6.
CREATE OR REPLACE FUNCTION memory_conflict_backlog(
    p_tenant text, p_floor double precision)
RETURNS bigint LANGUAGE sql STABLE AS $$
  SELECT count(*) FROM memory m
   WHERE m.tenant_id = p_tenant
     AND m.superseded_by IS NULL
     AND (m.occurred_until IS NULL OR m.occurred_until > now())
     AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                      WHERE s.memory_id = m.id AND s.floor <= p_floor)
$$;

-- A row whose vector, namespace, tenant or model changed: its pairs no longer describe it. The
-- exclusive lock waits out every scan in flight in the tenant (4.2), so none of them can insert a
-- pair computed on the old vector after this DELETE and leave it there. The notification wakes the
-- sweeper to rescan the row once this transaction commits.
CREATE OR REPLACE FUNCTION memory_conflict_forget_row() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  -- Both keys in one fixed order when the tenant changed, so two moves between the same two
  -- tenants cannot each hold one key and wait for the other.
  PERFORM pg_advisory_xact_lock(
    hashtextextended('memory_conflict:' || least(OLD.tenant_id, NEW.tenant_id), 0));
  IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
    PERFORM pg_advisory_xact_lock(
      hashtextextended('memory_conflict:' || greatest(OLD.tenant_id, NEW.tenant_id), 0));
  END IF;
  DELETE FROM memory_conflict WHERE older_id = NEW.id OR newer_id = NEW.id;
  DELETE FROM memory_conflict_scan WHERE memory_id = NEW.id;
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_moved
  AFTER UPDATE OF tenant_id, namespace, embedding, embedding_model ON memory
  FOR EACH ROW
  WHEN (OLD.tenant_id IS DISTINCT FROM NEW.tenant_id
     OR OLD.namespace IS DISTINCT FROM NEW.namespace
     OR OLD.embedding IS DISTINCT FROM NEW.embedding
     OR OLD.embedding_model IS DISTINCT FROM NEW.embedding_model)
  EXECUTE FUNCTION memory_conflict_forget_row();

-- A row that was not live and is live again: rows written while it was retired never paired with
-- it. Its old pairs stay; they are still true. The lock keeps a scan of this row that began while
-- it was retired from writing its mark after this DELETE.
CREATE OR REPLACE FUNCTION memory_conflict_rescan_row() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  PERFORM pg_advisory_xact_lock(hashtextextended('memory_conflict:' || NEW.tenant_id, 0));
  DELETE FROM memory_conflict_scan WHERE memory_id = NEW.id;
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_revived
  AFTER UPDATE OF superseded_by, occurred_until ON memory
  FOR EACH ROW
  WHEN ((OLD.superseded_by IS NOT NULL
         OR (OLD.occurred_until IS NOT NULL AND OLD.occurred_until <= now()))
    AND NEW.superseded_by IS NULL
    AND (NEW.occurred_until IS NULL OR NEW.occurred_until > now()))
  EXECUTE FUNCTION memory_conflict_rescan_row();

-- Wakes the sweeper for this row's tenant. Sends the tenant id only: a listener learns that some
-- row in the tenant waits for a scan, never which row or what it says.
--
-- Postgres delivers a notification only when the transaction commits, drops it on rollback, and
-- folds identical notifications inside one transaction into one. A bulk insert in one transaction
-- sends one per tenant; a rolled-back insert sends none; the sweeper never hears of a row before
-- the row is visible to it.
--
-- TRAP: never scan here and never write memory_conflict here. A scan inside this trigger runs
-- inside the inserting transaction, where two concurrent writes miss each other (section 6), and a
-- bulk insert pays a namespace scan per row inside the caller's statement. The catalog test in
-- tests/conflict_pairs.rs fails if any trigger function on memory calls memory_conflict_record or
-- inserts into memory_conflict.
CREATE OR REPLACE FUNCTION memory_conflict_wake() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_wake
  AFTER INSERT ON memory
  FOR EACH ROW
  WHEN (NEW.superseded_by IS NULL)
  EXECUTE FUNCTION memory_conflict_wake();
