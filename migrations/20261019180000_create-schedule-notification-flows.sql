-- Reusable texts and emails to customers about their appointments.
CREATE TABLE schedule_notification_templates (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(100) NOT NULL,
    channel ENUM('sms', 'email') NOT NULL,
    subject VARCHAR(255) NULL DEFAULT NULL,
    body TEXT NOT NULL,
    created_by INT NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    -- Migration only: the rule the template was made from.
    legacy_rule_id INT NULL DEFAULT NULL,
    INDEX idx_schedule_notification_templates_company (company_id, deleted_at),
    CONSTRAINT fk_schedule_notification_templates_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_templates_created_by
        FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);

-- Templates in a sequence for a calendar's appointments ("1 day before:
-- disconnect the sink; 1 hour before: see you soon"). The calendar's default
-- flow goes on every event of it; the others only on events that pick them.
CREATE TABLE schedule_notification_flows (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    calendar_id INT NOT NULL,
    name VARCHAR(100) NOT NULL,
    is_default TINYINT(1) NOT NULL DEFAULT 0,
    is_active TINYINT(1) NOT NULL DEFAULT 1,
    created_by INT NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    INDEX idx_schedule_notification_flows_company (company_id, deleted_at),
    CONSTRAINT fk_schedule_notification_flows_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_flows_calendar
        FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_flows_created_by
        FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);

-- The rules so far: each one's message becomes a template, and each
-- calendar's rules its default flow.
DELETE FROM schedule_notification_rules WHERE deleted_at IS NOT NULL;
INSERT INTO schedule_notification_templates
    (company_id, name, channel, subject, body, created_by, created_at, legacy_rule_id)
SELECT company_id, 'Reminder', channel, subject, body, created_by, created_at, id
  FROM schedule_notification_rules;
INSERT INTO schedule_notification_flows
    (company_id, calendar_id, name, is_default, is_active, created_by, created_at)
SELECT company_id, calendar_id, 'Reminders', 1, MAX(is_active), MIN(created_by), MIN(created_at)
  FROM schedule_notification_rules
 GROUP BY company_id, calendar_id;

-- Each rule is now a step of a flow: a template sent at a time before.
ALTER TABLE schedule_notification_rules
    ADD COLUMN flow_id INT NULL AFTER company_id,
    ADD COLUMN template_id INT NULL AFTER flow_id;
UPDATE schedule_notification_rules r
  JOIN schedule_notification_flows f
    ON f.company_id = r.company_id AND f.calendar_id = r.calendar_id
  JOIN schedule_notification_templates m ON m.legacy_rule_id = r.id
   SET r.flow_id = f.id, r.template_id = m.id;
ALTER TABLE schedule_notification_templates DROP COLUMN legacy_rule_id;
ALTER TABLE schedule_notification_rules
    DROP FOREIGN KEY fk_schedule_notification_rules_calendar,
    DROP COLUMN calendar_id,
    DROP COLUMN is_active,
    DROP COLUMN channel,
    DROP COLUMN subject,
    DROP COLUMN body,
    MODIFY flow_id INT NOT NULL,
    MODIFY template_id INT NOT NULL,
    ADD CONSTRAINT fk_schedule_notification_rules_flow
        FOREIGN KEY (flow_id) REFERENCES schedule_notification_flows(id) ON DELETE CASCADE,
    ADD CONSTRAINT fk_schedule_notification_rules_template
        FOREIGN KEY (template_id) REFERENCES schedule_notification_templates(id) ON DELETE CASCADE;

-- The calendar's default flow turned off for one event.
CREATE TABLE event_notification_skips (
    event_id INT NOT NULL,
    flow_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (event_id, flow_id),
    CONSTRAINT fk_event_notification_skips_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_notification_skips_flow
        FOREIGN KEY (flow_id) REFERENCES schedule_notification_flows(id) ON DELETE CASCADE
);

-- Another flow of the calendar picked for one event.
CREATE TABLE event_notification_picks (
    event_id INT NOT NULL,
    flow_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (event_id, flow_id),
    CONSTRAINT fk_event_notification_picks_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_notification_picks_flow
        FOREIGN KEY (flow_id) REFERENCES schedule_notification_flows(id) ON DELETE CASCADE
);
