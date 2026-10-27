-- Activity names a user removed from their suggestions under the activity
-- name input: never suggested to that user again, however often it is used.
CREATE TABLE deal_activity_suggestion_hides (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uq_deal_activity_suggestion_hides (company_id, user_id, name)
);
