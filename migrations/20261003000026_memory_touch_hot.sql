-- Let the touch after every search update its rows in place. Issue 72.
--
-- `touch_accessed` writes `access_count` and `last_accessed_at` on every row a search returns.
-- Postgres takes the HOT path, which writes no index entry, only when no indexed column changes.
-- A column named in a partial index predicate counts as indexed, so `memory_never_accessed` from
-- migration 005 sent every touch down the other path: a new entry in every index on `memory`, the
-- HNSW graph included, and a dead one for vacuum. Measured on a 5,000-row scratch store on
-- Postgres 17 and pgvector 0.8.6, 300 searches touching 8 rows each: 2,400 updates and 0 HOT with
-- the index, 1,993 HOT without it at the old fillfactor, all 2,400 HOT at fillfactor 90 on pages
-- written after the change.
--
-- The one reader the index served is the review queue's stale source. It now walks
-- `memory_created_at` oldest first and filters, so its cost tracks how many old rows were read
-- before it finds a page of unread ones. Someone opens that queue by hand; the index charged every
-- search a write to every index.
--
-- Trap for whoever adds the next index on `memory`: naming `last_accessed_at` or `access_count`
-- anywhere in it, key or predicate, puts this cost back. `tests/search_touch_hot.rs` reads the
-- catalog for that.
DROP INDEX IF EXISTS memory_never_accessed;

-- Room on each page for the new row version, which HOT needs on the same page as the old one.
-- Applies to pages written from here on. Existing pages keep what free space they have until a
-- rewrite, which this migration does not force: VACUUM FULL holds an exclusive lock for the length
-- of the copy and a migration has no business taking one. The index drop above is the larger share
-- of the gain on its own.
ALTER TABLE memory SET (fillfactor = 90);
