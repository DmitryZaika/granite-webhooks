-- A rep's own RingCentral sign-in (My Account > Connect my RingCentral).
-- RingCentral lets a login text only from its own numbers, so the company's
-- connection can text only as the person who connected it; a rep's texts go
-- out with this login instead (`ringcentral_auth::access_token_as`). Tokens
-- are encrypted like `company.ringcentral_*`.
CREATE TABLE ringcentral_user_logins (
  user_id INT NOT NULL PRIMARY KEY,
  company_id INT NOT NULL,
  extension_id VARCHAR(64) NOT NULL,
  access_token BLOB NOT NULL,
  refresh_token BLOB NOT NULL,
  access_expires BIGINT NOT NULL,
  refresh_expires BIGINT NOT NULL,
  connected_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uniq_ringcentral_user_logins_extension (company_id, extension_id),
  KEY idx_ringcentral_user_logins_refresh (refresh_expires)
);
