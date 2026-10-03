-- Quote editor (Moraware-style drawing, price lists, revisions, emails).
-- Requires the existing quotes table.

ALTER TABLE quotes
    ADD COLUMN deal_id BIGINT UNSIGNED NULL,
    ADD COLUMN status VARCHAR(20) NOT NULL DEFAULT 'draft',
    ADD COLUMN salesperson_id INT NULL,
    ADD COLUMN created_by INT NULL,
    ADD COLUMN price_list_id INT NULL,
    ADD COLUMN price_list_revision_id INT NULL,
    ADD COLUMN expiration_date DATE NULL,
    ADD COLUMN notes TEXT NULL,
    ADD COLUMN tax_pct DECIMAL(6, 3) NOT NULL DEFAULT 0,
    ADD COLUMN discount_pct DECIMAL(6, 3) NOT NULL DEFAULT 0,
    ADD COLUMN payment_terms_json JSON NULL,
    ADD COLUMN address VARCHAR(500) NULL,
    ADD COLUMN phone VARCHAR(50) NULL,
    ADD COLUMN email VARCHAR(255) NULL,
    ADD COLUMN draft_drawing_json JSON NULL,
    ADD COLUMN current_step TINYINT NOT NULL DEFAULT 1,
    ADD COLUMN current_revision INT NOT NULL DEFAULT 0,
    ADD COLUMN portal_token VARCHAR(64) NULL,
    ADD COLUMN signed_at DATETIME NULL,
    ADD COLUMN signature_data MEDIUMTEXT NULL,
    ADD COLUMN customer_accepted_option_id INT NULL,
    ADD COLUMN updated_at DATETIME NULL,
    ADD UNIQUE KEY uq_quotes_portal_token (portal_token),
    ADD KEY idx_quotes_company_created (company_id, created_date),
    ADD KEY idx_quotes_deal (deal_id);

CREATE TABLE quote_revisions (
    id INT AUTO_INCREMENT PRIMARY KEY,
    quote_id INT NOT NULL,
    revision_number INT NOT NULL,
    drawing_json JSON NOT NULL,
    options_json JSON NULL,
    totals_json JSON NULL,
    price_list_revision_id INT NULL,
    notes VARCHAR(500) NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uq_quote_revision (quote_id, revision_number),
    FOREIGN KEY (quote_id) REFERENCES quotes(id)
);

CREATE TABLE quote_autosaves (
    quote_id INT NOT NULL,
    user_id INT NOT NULL,
    drawing_json JSON NOT NULL,
    saved_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (quote_id, user_id),
    FOREIGN KEY (quote_id) REFERENCES quotes(id)
);

CREATE TABLE quote_emails (
    id INT AUTO_INCREMENT PRIMARY KEY,
    quote_id INT NOT NULL,
    revision_number INT NOT NULL,
    token VARCHAR(64) NOT NULL,
    recipients VARCHAR(1000) NOT NULL,
    subject VARCHAR(500) NOT NULL,
    form VARCHAR(100) NULL,
    sent_by INT NULL,
    sent_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at DATE NULL,
    view_count INT NOT NULL DEFAULT 0,
    first_viewed_at DATETIME NULL,
    last_viewed_at DATETIME NULL,
    UNIQUE KEY uq_quote_email_token (token),
    KEY idx_quote_emails_quote (quote_id),
    FOREIGN KEY (quote_id) REFERENCES quotes(id)
);

CREATE TABLE price_lists (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'active',
    sequence INT NOT NULL DEFAULT 0,
    current_revision_id INT NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    KEY idx_price_lists_company (company_id),
    FOREIGN KEY (company_id) REFERENCES company(id)
);

CREATE TABLE price_list_revisions (
    id INT AUTO_INCREMENT PRIMARY KEY,
    price_list_id INT NOT NULL,
    revision_number INT NOT NULL,
    info_json JSON NOT NULL,
    notes VARCHAR(500) NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uq_price_list_revision (price_list_id, revision_number),
    FOREIGN KEY (price_list_id) REFERENCES price_lists(id)
);
