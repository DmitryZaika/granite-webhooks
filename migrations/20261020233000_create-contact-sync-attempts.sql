-- When the contact-sync job last pushed a customer to a provider's contacts.
-- A customer whose sync keeps failing waits a day before the next try, so it
-- never holds up the rest of the queue.
CREATE TABLE contact_sync_attempts (
    customer_id INT NOT NULL,
    provider VARCHAR(16) NOT NULL,
    attempted_at DATETIME NOT NULL,
    PRIMARY KEY (customer_id, provider)
);
