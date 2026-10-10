-- Slot B and the per-unit pointer. Engine decision 0027. Readers pick embedding or embedding_b from
-- embedding_state.active_slot; a model change fills the inactive slot and moves the pointer, so no
-- vector ever moves between columns. Writers place each vector in its slot themselves.

ALTER TABLE memory
  ADD COLUMN IF NOT EXISTS embedding_b       vector(768),
  ADD COLUMN IF NOT EXISTS embedding_b_model text;

-- Built while the column is empty: one scan of memory, nothing per row. Rows then enter it as the
-- fill and dual writes land, which avoids a CREATE INDEX CONCURRENTLY on a live store. Same build
-- settings as memory_embedding_hnsw (20260819000003_hnsw_recall.sql).
CREATE INDEX IF NOT EXISTS memory_embedding_b_hnsw
  ON memory USING hnsw (embedding_b vector_cosine_ops)
  WITH (m = 16, ef_construction = 128);

CREATE TABLE IF NOT EXISTS embedding_state (
  tenant_id   text PRIMARY KEY DEFAULT 'me',
  active_slot text NOT NULL DEFAULT 'a' CHECK (active_slot IN ('a', 'b')),
  model_a     text,
  model_b     text,
  flipped_at  timestamptz,
  updated_at  timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT embedding_state_active_named
    CHECK (CASE active_slot WHEN 'a' THEN model_a IS NOT NULL ELSE model_b IS NOT NULL END),
  CONSTRAINT embedding_state_two_models CHECK (model_a IS DISTINCT FROM model_b)
);

-- The switch command's intent, one row per instance (decision 0027). `lumberroom-server embeddings`
-- writes target, flip, retire and verb under a row lock and bumps generation. In command mode the
-- sweep reads the row each pass and publishes what it applied; in env mode nothing touches it.
CREATE TABLE IF NOT EXISTS embedding_control (
  singleton          boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  generation         bigint NOT NULL DEFAULT 0,
  target             text,
  flip               boolean NOT NULL DEFAULT false,
  retire             text,
  verb               text CHECK (verb IN ('start', 'flip', 'rollback', 'retire')),
  requested_at       timestamptz,
  applied_generation bigint NOT NULL DEFAULT 0,
  server_models      text[] NOT NULL DEFAULT '{}',
  server_status      jsonb,
  server_seen_at     timestamptz,
  CONSTRAINT embedding_control_retire_not_target CHECK (retire IS DISTINCT FROM target)
);
INSERT INTO embedding_control (singleton) VALUES (true) ON CONFLICT DO NOTHING;

-- The conflict scan reads the unit's active slot. A forward replacement of
-- 20261004000026_conflict_pairs.sql; a deployment that replaced it with a SECURITY DEFINER version
-- re-applies its own after this file, as that file's header says.
CREATE OR REPLACE FUNCTION memory_conflict_record(
    p_tenant text, p_id uuid, p_floor double precision)
RETURNS void LANGUAGE plpgsql SET lock_timeout = '200ms' AS $$
DECLARE v_slot text;
BEGIN
  PERFORM pg_advisory_xact_lock_shared(hashtextextended('memory_conflict:' || p_tenant, 0));
  SELECT active_slot INTO v_slot FROM embedding_state WHERE tenant_id = p_tenant;
  IF coalesce(v_slot, 'a') = 'b' THEN
    WITH anchor AS MATERIALIZED (
      SELECT id, namespace, created_at, embedding_b AS v
        FROM memory
       WHERE tenant_id = p_tenant AND id = p_id AND embedding_b IS NOT NULL
         AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
    ), near AS MATERIALIZED (
      SELECT m.id, m.created_at, a.id AS anchor_id, a.created_at AS anchor_created_at,
             (1 - (m.embedding_b <=> a.v))::float8 AS similarity
        FROM anchor a
        JOIN memory m
          ON m.tenant_id = p_tenant AND m.namespace = a.namespace AND m.id <> a.id
       WHERE m.superseded_by IS NULL
         AND (m.occurred_until IS NULL OR m.occurred_until > now())
         AND m.embedding_b IS NOT NULL
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
  ELSE
    WITH anchor AS MATERIALIZED (
      SELECT id, namespace, created_at, embedding AS v
        FROM memory
       WHERE tenant_id = p_tenant AND id = p_id AND embedding IS NOT NULL
         AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
    ), near AS MATERIALIZED (
      SELECT m.id, m.created_at, a.id AS anchor_id, a.created_at AS anchor_created_at,
             (1 - (m.embedding <=> a.v))::float8 AS similarity
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
  END IF;

  INSERT INTO memory_conflict_scan (memory_id, tenant_id, floor)
  SELECT id, tenant_id, p_floor FROM memory WHERE tenant_id = p_tenant AND id = p_id
  ON CONFLICT (memory_id) DO UPDATE SET floor = EXCLUDED.floor, scanned_at = now();
END
$$;

-- A row's pairs stop describing it when it moves tenant or namespace, or when the vector in the slot
-- its unit reads changes. TG_ARGV[0] names the slot the calling trigger watches. Filling or retiring
-- the inactive slot returns at once, so a fill clears no pair and queues no rescan. A unit with no
-- state row reads slot A. Body otherwise as memory_conflict_forget_row (20261004000026), which
-- stays defined for deployments that replaced it.
CREATE OR REPLACE FUNCTION memory_conflict_forget_slot() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.tenant_id IS NOT DISTINCT FROM OLD.tenant_id
     AND NEW.namespace IS NOT DISTINCT FROM OLD.namespace
     AND coalesce((SELECT active_slot FROM embedding_state WHERE tenant_id = NEW.tenant_id), 'a')
         <> TG_ARGV[0] THEN
    RETURN NULL;
  END IF;
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

-- The WHEN clause is 20261005000028's, unchanged; only the function moves.
DROP TRIGGER IF EXISTS memory_conflict_moved ON memory;
CREATE TRIGGER memory_conflict_moved
  AFTER UPDATE OF tenant_id, namespace, embedding, embedding_model ON memory
  FOR EACH ROW
  WHEN (OLD.tenant_id IS DISTINCT FROM NEW.tenant_id
     OR OLD.namespace IS DISTINCT FROM NEW.namespace
     OR (OLD.embedding IS NULL) <> (NEW.embedding IS NULL)
     OR OLD.embedding <> NEW.embedding
     OR OLD.embedding_model IS DISTINCT FROM NEW.embedding_model)
  EXECUTE FUNCTION memory_conflict_forget_slot('a');

-- OPERATOR(public.<>), never a bare IS DISTINCT FROM on a vector: pg_dump prints that unqualified
-- and pg_restore runs with an empty search_path (fork 20261005900144 records the trap). Text
-- comparisons are safe.
DROP TRIGGER IF EXISTS memory_conflict_moved_b ON memory;
CREATE TRIGGER memory_conflict_moved_b
  AFTER UPDATE OF embedding_b, embedding_b_model ON memory
  FOR EACH ROW
  WHEN ((OLD.embedding_b IS NULL) <> (NEW.embedding_b IS NULL)
        OR OLD.embedding_b OPERATOR(public.<>) NEW.embedding_b
        OR OLD.embedding_b_model IS DISTINCT FROM NEW.embedding_b_model)
  EXECUTE FUNCTION memory_conflict_forget_slot('b');
