ALTER TABLE company
    ADD COLUMN billing_required TINYINT(1) NOT NULL DEFAULT 0;

CREATE TABLE company_billing (
    company_id INT NOT NULL PRIMARY KEY,
    stripe_customer_id VARCHAR(255) NULL,
    stripe_subscription_id VARCHAR(255) NULL,
    stripe_price_id VARCHAR(255) NULL,
    stripe_subscription_item_id VARCHAR(255) NULL,
    stripe_payment_method_id VARCHAR(255) NULL,
    subscription_status VARCHAR(32) NOT NULL DEFAULT 'none',
    payment_status VARCHAR(32) NOT NULL DEFAULT 'none',
    current_period_start DATETIME NULL,
    current_period_end DATETIME NULL,
    last_successful_payment_at DATETIME NULL,
    failed_payment_at DATETIME NULL,
    grace_period_ends_at DATETIME NULL,
    plan_name VARCHAR(255) NULL,
    monthly_price_cents INT NULL,
    user_count INT NULL,
    outstanding_balance_cents INT NOT NULL DEFAULT 0,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_company_billing_customer (stripe_customer_id),
    UNIQUE KEY uniq_company_billing_subscription (stripe_subscription_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE company_billing_invoices (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    stripe_invoice_id VARCHAR(255) NOT NULL,
    status VARCHAR(32) NOT NULL,
    amount_due_cents INT NOT NULL DEFAULT 0,
    amount_paid_cents INT NOT NULL DEFAULT 0,
    currency VARCHAR(8) NOT NULL DEFAULT 'usd',
    hosted_invoice_url VARCHAR(1024) NULL,
    invoice_pdf VARCHAR(1024) NULL,
    paid_at DATETIME NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_company_billing_invoice (stripe_invoice_id),
    KEY idx_company_billing_invoices_company (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE stripe_webhook_events (
    stripe_event_id VARCHAR(255) NOT NULL PRIMARY KEY,
    event_type VARCHAR(128) NOT NULL,
    processed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE billing_notifications (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    dedupe_key VARCHAR(191) NOT NULL,
    title VARCHAR(255) NOT NULL,
    message TEXT NOT NULL,
    action_label VARCHAR(255) NOT NULL,
    href VARCHAR(255) NOT NULL,
    is_done TINYINT(1) NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_billing_notification (company_id, user_id, dedupe_key),
    KEY idx_billing_notifications_user (user_id, is_done)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
