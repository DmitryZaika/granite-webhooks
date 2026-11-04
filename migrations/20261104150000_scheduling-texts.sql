-- Admin > Schedule > Customer notifications > Scheduling texts (the
-- `confirmation` templates): each one can be for some calendars only. A
-- template with no rows here is for every calendar.
CREATE TABLE schedule_notification_template_calendars (
    template_id INT NOT NULL,
    calendar_id INT NOT NULL,
    PRIMARY KEY (template_id, calendar_id),
    CONSTRAINT fk_schedule_template_calendars_template
        FOREIGN KEY (template_id) REFERENCES schedule_notification_templates(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_template_calendars_calendar
        FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE
);

-- "If the customer doesn't reply": a scheduling text can send a follow-up
-- (itself again, or another scheduling text) this many hours later, only
-- within the company's texting hours.
ALTER TABLE schedule_notification_templates
    ADD COLUMN followup_template_id INT NULL DEFAULT NULL,
    ADD COLUMN followup_after_hours INT NULL DEFAULT NULL,
    ADD CONSTRAINT fk_schedule_templates_followup
        FOREIGN KEY (followup_template_id) REFERENCES schedule_notification_templates(id) ON DELETE SET NULL;

-- `schedule_ai_ready` ("Mark Ready when the customer agrees"): after a
-- scheduling text, AI reads the customer's replies and, when it is certain
-- they agreed to the date, turns the job Ready and tells the Scheduling
-- Managers. Off for every company.
-- Texting hours: follow-ups go out only on these days (bit 0 Monday ... bit
-- 6 Sunday; 63 = Monday to Saturday) between these store times.
ALTER TABLE company
    ADD COLUMN schedule_ai_ready TINYINT(1) NOT NULL DEFAULT 0,
    ADD COLUMN schedule_text_days TINYINT UNSIGNED NOT NULL DEFAULT 63,
    ADD COLUMN schedule_text_start TIME NOT NULL DEFAULT '09:00:00',
    ADD COLUMN schedule_text_end TIME NOT NULL DEFAULT '18:00:00';

-- A scheduling text sent to a customer, waiting for their answer: the AI
-- reads the replies (with the switch on) and the follow-up goes out when
-- there is none. `checked_through`: the newest reply the AI has read.
CREATE TABLE schedule_text_answers (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    event_id INT NOT NULL,
    -- The last text sent: its follow-up is the next one.
    template_id INT NULL DEFAULT NULL,
    -- The appointment's start when the first text went out: moving it ends the wait.
    start_date DATETIME NOT NULL,
    phone CHAR(10) NOT NULL,
    -- What the customer was sent, first text first.
    body TEXT NOT NULL,
    sent_by INT NULL DEFAULT NULL,
    -- The first text: replies count from here.
    sent_at DATETIME NOT NULL,
    -- The last text (first or follow-up): "no reply" counts from here.
    last_sent_at DATETIME NOT NULL,
    followups_sent INT NOT NULL DEFAULT 0,
    followup_due_at DATETIME NULL DEFAULT NULL,
    status ENUM('waiting', 'agreed', 'not_agreed', 'closed') NOT NULL DEFAULT 'waiting',
    checked_through DATETIME NULL DEFAULT NULL,
    evidence VARCHAR(255) NULL DEFAULT NULL,
    decided_at DATETIME NULL DEFAULT NULL,
    INDEX idx_schedule_text_answers_waiting (status, sent_at),
    INDEX idx_schedule_text_answers_event (event_id),
    CONSTRAINT fk_schedule_text_answers_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_text_answers_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_text_answers_template
        FOREIGN KEY (template_id) REFERENCES schedule_notification_templates(id) ON DELETE SET NULL,
    CONSTRAINT fk_schedule_text_answers_sent_by
        FOREIGN KEY (sent_by) REFERENCES users(id) ON DELETE SET NULL
);
