-- Files attached to a quote: plans and photos from the customer, and the
-- original PDF of a quote moved over from Moraware CounterGo.
CREATE TABLE quote_files (
    id INT AUTO_INCREMENT PRIMARY KEY,
    quote_id INT NOT NULL,
    url VARCHAR(1000) NOT NULL,
    name VARCHAR(255) NOT NULL,
    created_by VARCHAR(100) NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    KEY idx_quote_files_quote (quote_id),
    FOREIGN KEY (quote_id) REFERENCES quotes(id)
);
