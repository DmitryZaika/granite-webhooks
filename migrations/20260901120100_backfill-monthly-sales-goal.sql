ALTER TABLE users
    ADD COLUMN monthly_sales_goal DECIMAL(12, 2) NULL AFTER ringcentral_phone_number;


UPDATE users
SET monthly_sales_goal = 50000
WHERE monthly_sales_goal IS NULL
  AND is_deleted = 0;
