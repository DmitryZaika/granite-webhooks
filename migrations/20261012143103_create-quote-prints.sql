-- One row per time a quote is printed from the quote page, so the deal's
-- history can show "Quote printed" next to "Quote sent".
CREATE TABLE quote_prints (
    id INT AUTO_INCREMENT PRIMARY KEY,
    quote_id INT NOT NULL,
    revision_number INT NOT NULL,
    form VARCHAR(100) NULL,
    printed_by INT NULL,
    printed_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    KEY idx_quote_prints_quote (quote_id, printed_at)
);
