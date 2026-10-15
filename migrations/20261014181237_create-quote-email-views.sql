-- One row per time a customer opens an emailed quote link, so the quote
-- page can list every view (quote_emails keeps the counters).
CREATE TABLE quote_email_views (
    id INT AUTO_INCREMENT PRIMARY KEY,
    quote_email_id INT NOT NULL,
    viewed_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    ip_address VARCHAR(64) NULL,
    user_agent VARCHAR(1000) NULL,
    KEY idx_quote_email_views_email (quote_email_id, viewed_at),
    FOREIGN KEY (quote_email_id) REFERENCES quote_emails(id) ON DELETE CASCADE
);
