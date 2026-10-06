-- The per-call recall log: one row per memory one memory_search or context_bootstrap returned.
--
-- recall_emission keeps one aggregate row per (tenant, content digest, memory, tool), so it can say
-- when a fact first and last went out and nothing about which rows one call returned together. An
-- offline evaluation of digest ranking has to approximate its labels because of that. This table
-- answers the question directly, and only when RECALL_EVENT_LOG is on.
--
-- No content, no content digest, no query text. The row is ids, positions and times, so it adds no
-- plaintext exposure and none of the verification-oracle concern migration 20260823000017 removed
-- from recall_emission.
--
-- tenant_id has no default. The server binds it on every insert, and a default would file one
-- tenant's calls under another's name.
--
-- No unique key on (call_id, memory_id): the digest's recent section can repeat a row the profile or
-- project section already returned, and both appearances are part of the record. The identity
-- column is the key, and it gives the retention purge a cheap handle to delete by.
CREATE TABLE IF NOT EXISTS recall_event (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tenant_id  text        NOT NULL,
  call_id    uuid        NOT NULL,
  tool       text        NOT NULL,
  client     text        NOT NULL,
  session_id text,
  project    text,
  memory_id  uuid        NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  namespace  text        NOT NULL,
  section    text,
  rank       integer     NOT NULL CHECK (rank >= 1),
  emitted_at timestamptz NOT NULL DEFAULT now()
);

-- The evaluation's window read, and the retention purge.
CREATE INDEX IF NOT EXISTS recall_event_tenant_emitted
  ON recall_event (tenant_id, emitted_at);

-- The evaluation's per-row read, and the cascade. memory_id leads because the cascade a forget
-- triggers filters on memory_id alone, and a tenant-first index cannot serve that without scanning
-- the whole index. Memory ids are unique across tenants, so the tenant column costs nothing here.
CREATE INDEX IF NOT EXISTS recall_event_memory
  ON recall_event (memory_id, tenant_id);
