-- Horus (FusionDrive) read-only inventory integration
ALTER TABLE company ADD COLUMN horus_license_id VARCHAR(255) NULL;
ALTER TABLE company ADD COLUMN horus_password_enc TEXT NULL;
ALTER TABLE company ADD COLUMN horus_last_change_id BIGINT NOT NULL DEFAULT 1;
ALTER TABLE company ADD COLUMN horus_synced_at DATETIME NULL;

CREATE TABLE horus_slabs (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  horus_slab_id INT NOT NULL,
  reference VARCHAR(255) NULL,
  block_name VARCHAR(255) NULL,
  job_id VARCHAR(255) NULL,
  width DECIMAL(12, 4) NULL,
  height DECIMAL(12, 4) NULL,
  receipt_width DECIMAL(12, 4) NULL,
  receipt_height DECIMAL(12, 4) NULL,
  reg_date VARCHAR(64) NULL,
  parent_slab_id INT NULL,
  is_remnant TINYINT NOT NULL DEFAULT 0,
  cost_per_m2 DECIMAL(12, 4) NULL,
  thumbnail TEXT NULL,
  match_url TEXT NULL,
  status INT NULL,
  stone_type_name VARCHAR(255) NULL,
  stone_name VARCHAR(255) NULL,
  class_name VARCHAR(255) NULL,
  finishing_name VARCHAR(255) NULL,
  thickness_name VARCHAR(255) NULL,
  stone_id INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY uniq_horus_slabs_company_horus_id (company_id, horus_slab_id),
  INDEX idx_horus_slabs_company_stone (company_id, stone_id),
  INDEX idx_horus_slabs_company_stone_name (company_id, stone_name),
  CONSTRAINT fk_horus_slabs_company
    FOREIGN KEY (company_id) REFERENCES company (id)
    ON DELETE CASCADE,
  CONSTRAINT fk_horus_slabs_stone
    FOREIGN KEY (stone_id) REFERENCES stones (id)
    ON DELETE SET NULL
);
