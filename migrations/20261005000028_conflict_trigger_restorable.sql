-- memory_conflict_moved, recreated so a dump of the store restores.
--
-- Migration 20261004000026 compared the two vectors with IS DISTINCT FROM. pg_dump writes that back
-- as IS DISTINCT FROM with no schema on the operator it hides, and pg_restore runs with search_path
-- empty, so the restore looks for `=` on vector in pg_catalog alone and fails:
-- `operator does not exist: public.vector = public.vector`. pg_restore --exit-on-error stops there;
-- without it the restore finishes and the trigger is missing, so a changed vector keeps its old
-- pairs and never rescans.
--
-- A plain operator survives. Postgres stores the operator itself, and pg_dump prints it as
-- OPERATOR(public.<>), naming whichever schema holds pgvector. The NULL test carries the half of
-- IS DISTINCT FROM that `<>` drops: a vector set or cleared fires, two NULLs do not. The three text
-- comparisons stay as they were; text equality lives in pg_catalog and restores.
--
-- A dump taken before this migration still holds the old text, and restores with the trigger
-- missing unless --exit-on-error is dropped. Restore it without that flag, then start the server:
-- the dump's _sqlx_migrations lacks this version, so boot runs this file and the trigger comes back.
-- tests/trigger_restore.rs replays every trigger's dumped text under an empty search_path and names
-- any that fails.
--
-- memory_conflict_forget_row stays as it is: only the WHEN clause was wrong, and a deployment that
-- replaced the function keeps its replacement.
DROP TRIGGER IF EXISTS memory_conflict_moved ON memory;
CREATE TRIGGER memory_conflict_moved
  AFTER UPDATE OF tenant_id, namespace, embedding, embedding_model ON memory
  FOR EACH ROW
  WHEN (OLD.tenant_id IS DISTINCT FROM NEW.tenant_id
     OR OLD.namespace IS DISTINCT FROM NEW.namespace
     OR (OLD.embedding IS NULL) <> (NEW.embedding IS NULL)
     OR OLD.embedding <> NEW.embedding
     OR OLD.embedding_model IS DISTINCT FROM NEW.embedding_model)
  EXECUTE FUNCTION memory_conflict_forget_row();
