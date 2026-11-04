-- How long a company's installations and templates take, from the sale's
-- size (Admin > Schedule > Job time). The schedule sizes new events and the
-- Auto-plan fills each person's work day with them.
-- `work_days` is a bit mask, bit 0 Sunday ... bit 6 Saturday (62 = Mon-Fri).
-- Day start and end are minutes after midnight in the shop's time zone.
CREATE TABLE schedule_time_rules (
    company_id INT NOT NULL,
    kind ENUM('installations', 'templates') NOT NULL,
    base_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_sqft_minutes DECIMAL(6,2) NOT NULL DEFAULT 0,
    per_waterfall_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_sink_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_cooktop_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_backsplash_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_full_backsplash_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    per_tear_out_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0,
    travel_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 30,
    day_start_minute SMALLINT UNSIGNED NOT NULL DEFAULT 540,
    day_end_minute SMALLINT UNSIGNED NOT NULL DEFAULT 1020,
    work_days TINYINT UNSIGNED NOT NULL DEFAULT 62,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (company_id, kind),
    CONSTRAINT fk_schedule_time_rules_company
        FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
);
