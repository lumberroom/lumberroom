-- Decision 0023. The name the owner gives a client, kept beside the name it registered with.
-- Null until the owner names it. Registration never writes this column.
ALTER TABLE oauth_client ADD COLUMN IF NOT EXISTS owner_label text;

-- A label belongs to a client the owner approved. Registration is anonymous, so this is the line
-- that keeps a stranger's client from arriving with a name the owner seems to have chosen.
ALTER TABLE oauth_client ADD CONSTRAINT oauth_client_label_needs_consent
  CHECK (owner_label IS NULL
         OR (consented_at IS NOT NULL AND octet_length(owner_label) BETWEEN 1 AND 200));
