-- Add migration script here
ALTER TABLE deals_images ADD COLUMN created_by VARCHAR(100) NULL;
ALTER TABLE deals_documents ADD COLUMN created_by VARCHAR(100) NULL;
ALTER TABLE deals ADD COLUMN created_by VARCHAR(100) NULL;
ALTER TABLE images ADD COLUMN created_by VARCHAR(100) NULL;
