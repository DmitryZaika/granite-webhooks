-- Add migration script here
-- 1. Create the feedback table linked to your existing history
CREATE TABLE message_feedback (
    id INT AUTO_INCREMENT PRIMARY KEY,
    history JSON NOT NULL,                 -- MySQL uses JSON instead of JSONB
    feedback_text TEXT,                    -- The reason why the user disliked it
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMP NULL DEFAULT NULL
);