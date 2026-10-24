-- One-time codes that unlock a customer's own page (`customer_view.rs`).
-- Attempts are counted here, not in the visitor's cookie: a cookie can be
-- replayed, which made the attempt limit unlimited. Rows stay a day after
-- they expire so sends per address can be limited per hour.
CREATE TABLE customer_view_codes (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    -- Random (hex), held by the visitor's signed cookie.
    token CHAR(64) NOT NULL,
    company_id INT NOT NULL,
    -- NULL when no customer of the company has the address: nothing was sent.
    customer_id INT NULL,
    -- Where the code went (lowercased).
    email VARCHAR(320) NOT NULL,
    -- sha256 of the code; NULL with customer_id.
    code_hash CHAR(64) NULL,
    attempts INT NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL,
    expires_at DATETIME NOT NULL,
    UNIQUE KEY customer_view_codes_token (token),
    KEY customer_view_codes_address (company_id, email, created_at),
    KEY customer_view_codes_expires (expires_at)
);
