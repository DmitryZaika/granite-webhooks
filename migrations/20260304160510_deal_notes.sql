CREATE TABLE deal_notes (
    id SERIAL PRIMARY KEY,
    deal_id BIGINT UNSIGNED NOT NULL,
    company_id INT NOT NULL,
    content TEXT NOT NULL,
    is_pinned TINYINT DEFAULT 0,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    created_by VARCHAR(255) NULL,
    deleted_at DATETIME NULL,
    FOREIGN KEY (deal_id) REFERENCES deals(id)
);

CREATE TABLE deal_note_comments (
    id SERIAL PRIMARY KEY,
    note_id BIGINT UNSIGNED NOT NULL,
    company_id INT NOT NULL,
    content TEXT NOT NULL,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    created_by VARCHAR(255) NULL,
    deleted_at DATETIME NULL,
    FOREIGN KEY (note_id) REFERENCES deal_notes(id)
);