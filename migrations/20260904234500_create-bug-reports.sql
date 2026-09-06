CREATE TABLE bug_reports (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    title VARCHAR(200) NOT NULL,
    description TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_bug_reports_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_bug_reports_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    INDEX idx_bug_reports_company_created (company_id, created_at)
);

CREATE TABLE bug_report_images (
    id INT AUTO_INCREMENT PRIMARY KEY,
    bug_report_id INT NOT NULL,
    company_id INT NOT NULL,
    url VARCHAR(700) NOT NULL,
    sort_order TINYINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_bug_report_images_report
        FOREIGN KEY (bug_report_id)
        REFERENCES bug_reports(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_bug_report_images_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    INDEX idx_bug_report_images_report (bug_report_id)
);
