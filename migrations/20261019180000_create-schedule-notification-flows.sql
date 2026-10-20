-- A calendar's notification rule turned off for one event.
CREATE TABLE event_notification_skips (
    event_id INT NOT NULL,
    rule_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (event_id, rule_id),
    CONSTRAINT fk_event_notification_skips_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_notification_skips_rule
        FOREIGN KEY (rule_id) REFERENCES schedule_notification_rules(id) ON DELETE CASCADE
);
