-- Per-company quote price lists. Each save stores an immutable revision so
-- existing quotes keep pricing against the revision they were created with.
CREATE TABLE IF NOT EXISTS price_lists (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    status ENUM('active', 'inactive') NOT NULL DEFAULT 'active',
    sequence INT NOT NULL DEFAULT 0,
    current_revision_id INT NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    INDEX idx_price_lists_company (company_id, deleted_at),
    FOREIGN KEY (company_id) REFERENCES company(id)
);

CREATE TABLE IF NOT EXISTS price_list_revisions (
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
