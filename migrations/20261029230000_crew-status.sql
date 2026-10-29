-- How the crew (the event's installer or template maker) is getting on with
-- it today: on_my_way (their button, or their phone leaving the shop or the
-- last job), arrived (their phone reached the address), completed (their
-- button). Each time is when it happened; on_my_way_sent_at is when the
-- customer was told. Moving the event to another day clears them.
ALTER TABLE events
    ADD COLUMN crew_status VARCHAR(16) NULL AFTER status,
    ADD COLUMN on_my_way_at DATETIME NULL AFTER crew_status,
    ADD COLUMN on_my_way_sent_at DATETIME NULL AFTER on_my_way_at,
    ADD COLUMN arrived_at DATETIME NULL AFTER on_my_way_sent_at,
    ADD COLUMN left_at DATETIME NULL AFTER arrived_at,
    ADD COLUMN completed_at DATETIME NULL AFTER left_at;

-- Where each crew member's phone last was, and the place they are at: the
-- shop, or one of their events (place_event_id).
CREATE TABLE crew_positions (
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    latitude DOUBLE NOT NULL,
    longitude DOUBLE NOT NULL,
    recorded_at DATETIME NOT NULL,
    at_shop TINYINT(1) NOT NULL DEFAULT 0,
    place_event_id INT NULL,
    place_latitude DOUBLE NULL,
    place_longitude DOUBLE NULL,
    place_since DATETIME NULL,
    PRIMARY KEY (user_id, company_id)
);

-- Admin > Schedule: what the customer gets when the crew taps On my way (a
-- text, or an email when the company cannot text). Nothing goes out while
-- it is empty.
ALTER TABLE company
    ADD COLUMN on_my_way_message TEXT NULL,
    ADD COLUMN on_my_way_subject VARCHAR(255) NULL;
