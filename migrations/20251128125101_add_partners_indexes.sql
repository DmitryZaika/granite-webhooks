-- Add migration script here
-- Add index for fast partner lookup (customers with company_name)
ALTER TABLE customers 
ADD INDEX idx_company_name_basic (company_name(100), deleted_at, company_id);
