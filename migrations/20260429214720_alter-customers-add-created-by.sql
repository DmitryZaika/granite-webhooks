-- Migration to add 'created_by' and 'assigned_by' columns to customers table
ALTER TABLE customers
    ADD COLUMN created_by VARCHAR(100) NULL