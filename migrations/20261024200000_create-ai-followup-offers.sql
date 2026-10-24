-- The company's current offer for AI follow-ups (`routes/followups/offer.rs`): a
-- real deal the writer may give as the reason to reply, until `ends_on`.
-- One row per company. Clearing the offer keeps the row with an empty text,
-- so `updated_at` still tells a running follow-up that copy written before
-- the change may name an offer that no longer stands.
CREATE TABLE IF NOT EXISTS ai_followup_offers (
    company_id INT NOT NULL PRIMARY KEY,
    offer VARCHAR(500) NOT NULL DEFAULT '',
    -- The last store day it may be mentioned; NULL: until it is changed.
    ends_on DATE NULL,
    updated_by INT NULL,
    -- UTC, set on every save.
    updated_at DATETIME NOT NULL
);
