CREATE TABLE calendars (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(100) NOT NULL,
    color VARCHAR(50) NOT NULL DEFAULT 'blue',
    created_by INT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    CONSTRAINT fk_calendars_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_calendars_created_by
        FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);

ALTER TABLE events
    ADD COLUMN location VARCHAR(500) NULL DEFAULT NULL AFTER description,
    ADD COLUMN calendar_id INT NULL DEFAULT NULL AFTER sale_id;

ALTER TABLE events
    ADD CONSTRAINT fk_events_calendar
        FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE SET NULL;

CREATE TABLE event_history (
    id INT AUTO_INCREMENT PRIMARY KEY,
    event_id INT NOT NULL,
    company_id INT NOT NULL,
    action VARCHAR(50) NOT NULL,
    message TEXT NOT NULL,
    created_by VARCHAR(255) NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_event_history_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_history_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

CREATE TABLE event_comments (
    id INT AUTO_INCREMENT PRIMARY KEY,
    event_id INT NOT NULL,
    company_id INT NOT NULL,
    content TEXT NOT NULL,
    created_by VARCHAR(255) NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    CONSTRAINT fk_event_comments_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_comments_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

CREATE TABLE event_attachments (
    id INT AUTO_INCREMENT PRIMARY KEY,
    event_id INT NOT NULL,
    company_id INT NOT NULL,
    url VARCHAR(512) NOT NULL,
    name VARCHAR(255) NOT NULL,
    kind VARCHAR(20) NOT NULL,
    created_by VARCHAR(255) NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    CONSTRAINT fk_event_attachments_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_attachments_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

CREATE TABLE event_images (
    id INT AUTO_INCREMENT PRIMARY KEY,
    event_id INT NOT NULL,
    company_id INT NOT NULL,
    url VARCHAR(512) NOT NULL,
    created_by VARCHAR(255) NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    CONSTRAINT fk_event_images_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_images_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);
