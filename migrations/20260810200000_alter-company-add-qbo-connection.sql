ALTER TABLE company ADD COLUMN qbo_realm_id VARCHAR(64);
ALTER TABLE company ADD COLUMN qbo_access_token BLOB;
ALTER TABLE company ADD COLUMN qbo_refresh_token BLOB;
ALTER TABLE company ADD COLUMN qbo_access_expires BIGINT;
ALTER TABLE company ADD COLUMN qbo_refresh_expires BIGINT;
ALTER TABLE company ADD COLUMN qbo_connected_at DATETIME;
ALTER TABLE company ADD COLUMN qbo_connected_by INT;
