-- Add migration script here
ALTER TABLE slab_inventory ADD COLUMN deleted_at TIMESTAMP NULL DEFAULT NULL;