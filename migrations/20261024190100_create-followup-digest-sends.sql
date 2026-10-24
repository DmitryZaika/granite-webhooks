-- One row per rep and store day the 9 AM "customers waiting on you" email
-- was looked at (`followups/digest.rs`): the claim, so a rep gets it once a
-- day however often the job runs.
CREATE TABLE IF NOT EXISTS followup_digest_sends (
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    sent_on DATE NOT NULL,
    -- UTC.
    sent_at DATETIME NOT NULL,
    PRIMARY KEY (user_id, sent_on)
);
