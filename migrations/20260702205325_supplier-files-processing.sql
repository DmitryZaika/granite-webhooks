ALTER TABLE supplier_files
  ADD COLUMN status ENUM('pending','processing','ready','failed') NOT NULL DEFAULT 'pending',
  ADD COLUMN status_updated_at TIMESTAMP NULL,
  ADD COLUMN attempts TINYINT NOT NULL DEFAULT 0,
  ADD COLUMN error_token VARCHAR(64) NULL,
  ADD COLUMN content_hash CHAR(64) NULL,
  ADD COLUMN pipeline_version TINYINT NOT NULL DEFAULT 0,
  ADD COLUMN processed_at TIMESTAMP NULL;

CREATE INDEX idx_supplier_files_status ON supplier_files (status, status_updated_at);

CREATE TABLE supplier_file_chunks (
  id SERIAL PRIMARY KEY,
  supplier_file_id BIGINT UNSIGNED NOT NULL,
  chunk_index INT NOT NULL,
  heading VARCHAR(255) NOT NULL DEFAULT '',
  content TEXT NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_file_chunk (supplier_file_id, chunk_index),
  CONSTRAINT fk_chunks_supplier_file FOREIGN KEY (supplier_file_id)
    REFERENCES supplier_files (id) ON DELETE CASCADE
);
