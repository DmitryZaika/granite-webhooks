-- Password reset links. The Remix app created this table on first use
-- (`passwordReset.server.ts`); the API needs it in every schema. Only the
-- SHA-256 of a link's token is stored.
CREATE TABLE IF NOT EXISTS password_reset_tokens (
  id         INT AUTO_INCREMENT PRIMARY KEY,
  user_id    INT          NOT NULL,
  token_hash VARCHAR(128) NOT NULL,
  expires_at DATETIME     NOT NULL,
  used       BOOLEAN      NOT NULL DEFAULT FALSE,
  created_at DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_token_hash (token_hash),
  INDEX idx_user_id (user_id),
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Failed sign-ins, for the login rate limits. Lambda instances share no
-- memory, so the limits are counted here. `email_hash` and `ip_hash` are
-- SHA-256 hex (no raw addresses kept); old rows are pruned on login.
CREATE TABLE IF NOT EXISTS login_attempts (
  id          BIGINT AUTO_INCREMENT PRIMARY KEY,
  email_hash  CHAR(64) NOT NULL,
  ip_hash     CHAR(64) NOT NULL,
  attempted_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_login_attempts_email (email_hash, attempted_at),
  KEY idx_login_attempts_ip (ip_hash, attempted_at),
  KEY idx_login_attempts_time (attempted_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
