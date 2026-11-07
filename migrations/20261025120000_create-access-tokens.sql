-- Access tokens for the webhooks Lambda: one way for every caller (the CRM,
-- lead-form partners, CloudTalk, RingCentral, Telegram, SES) to prove who it
-- is, replacing REMIX_KEY, WEBHOOK_SECRET and the shared bearer UUID.
--
-- Tokens are `gmk_` + 32 random bytes in base64url (47 characters), made by
-- `cargo run -p common --bin access-token`. Only the SHA-256 is stored, so a
-- copy of this table cannot call anything.
--
-- A token works on the routes of its scopes. One with a company_id works only
-- on routes whose path names that company; one without (internal callers)
-- works for every company. The Lambda caches a token for 60 seconds, so a
-- revocation takes at most a minute.
--
-- All dates are UTC.

CREATE TABLE IF NOT EXISTS access_tokens (
  id           BIGINT       NOT NULL AUTO_INCREMENT,
  -- who holds it, e.g. "Acme - Zapier lead form"
  name         VARCHAR(255) NOT NULL,
  -- NULL: internal, any company
  company_id   INT          NULL,
  scopes       SET('leads', 'internal', 'telephony', 'telegram', 'ses') NOT NULL,
  -- SHA-256 hex of the token; the token itself is never stored
  token_hash   CHAR(64)     NOT NULL,
  -- its last 4 characters, to tell tokens apart in lists and logs
  token_hint   CHAR(4)      NOT NULL,
  created_at   DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by   VARCHAR(255) NULL,
  -- NULL: never expires
  expires_at   DATETIME     NULL,
  revoked_at   DATETIME     NULL,
  -- written at most every 5 minutes per Lambda instance
  last_used_at DATETIME     NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_access_tokens_hash (token_hash),
  KEY idx_access_tokens_company (company_id),
  CONSTRAINT fk_access_tokens_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
