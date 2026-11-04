-- One-time links that let a port's authorized person e-sign its Letter of
-- Authorization without an account (Send to sign on a port's Documents
-- step). Only the token's SHA-256 is kept; a new link revokes the older
-- open ones of the port.
CREATE TABLE IF NOT EXISTS phone_port_loa_invites (
  id                 BIGINT        NOT NULL AUTO_INCREMENT,
  company_id         INT           NOT NULL,
  port_order_id      BIGINT        NOT NULL,
  token_hash         CHAR(64)      NOT NULL,
  signer_name        VARCHAR(100)  NOT NULL,
  signer_email       VARCHAR(255)  NOT NULL,
  created_by_user_id INT           NOT NULL,
  created_at         DATETIME(3)   NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  expires_at         DATETIME(3)   NOT NULL,
  signed_at          DATETIME(3)   NULL,
  revoked_at         DATETIME(3)   NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_port_loa_invite_token (token_hash),
  KEY idx_port_loa_invite_order (company_id, port_order_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
