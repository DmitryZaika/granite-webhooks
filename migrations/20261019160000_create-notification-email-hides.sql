-- Email threads a user removed from the notification bell without reading
-- them. A thread shows again once a message newer than hidden_at arrives.
CREATE TABLE notification_email_hides (
    user_id INT NOT NULL,
    thread_id VARCHAR(64) NOT NULL,
    hidden_at DATETIME NOT NULL,
    PRIMARY KEY (user_id, thread_id)
);
