-- Add migration script here
ALTER TABLE customers ADD COLUMN phone_2 VARCHAR(255) NULL AFTER phone;