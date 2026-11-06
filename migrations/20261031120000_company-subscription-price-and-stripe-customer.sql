-- The payments page (payments.granite-manager.com): each company's monthly
-- price, read when its subscription starts, and its Stripe customer, made on
-- its first checkout. Stripe keeps the subscriptions and payments themselves.
-- Neither column changes access to the CRM.
ALTER TABLE company
    ADD COLUMN subscription_price_cents INT NOT NULL DEFAULT 80000,
    ADD COLUMN stripe_customer_id VARCHAR(255) NULL,
    ADD UNIQUE KEY uniq_company_stripe_customer (stripe_customer_id);
