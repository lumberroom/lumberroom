-- A "both are fine" verdict on a conflict pair. The conflicts query joins against this so a pair
-- the owner has read and kept leaves the queue. Keyed on the two ids in uuid order: the pair is
-- unordered, and a row's text never changes under its id, so the dismissal never goes stale.
CREATE TABLE IF NOT EXISTS memory_pair_dismissed (
  tenant_id       text NOT NULL,
  lo_id           uuid NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  hi_id           uuid NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  -- Two columns because one is not enough to name anybody: a deployment that puts several people
  -- behind one client writes the same `dismissed_by` for all of them. `token_id` is the
  -- fingerprint the principal already documents as safe to log.
  dismissed_by    text NOT NULL,
  dismissed_token text NOT NULL,
  dismissed_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, lo_id, hi_id),
  CHECK (lo_id < hi_id)
);

-- The cascades probe each id column without a tenant, which the primary key cannot serve.
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_lo ON memory_pair_dismissed (lo_id);
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_hi ON memory_pair_dismissed (hi_id);
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_recent
  ON memory_pair_dismissed (tenant_id, dismissed_at DESC);
