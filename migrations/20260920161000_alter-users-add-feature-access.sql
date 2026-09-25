ALTER TABLE users
  ADD COLUMN is_signature_available TINYINT(1) NOT NULL DEFAULT 0;
