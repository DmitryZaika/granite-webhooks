-- Email participants: CC / BCC / reply-all / forward support.
--
-- Supersedes `20260717234741_separate-out-email.sql` on the `better-emails`
-- branch. Same table name, column names and type enum as that PR, so the two
-- are interchangeable from application code's point of view, with three
-- deliberate differences:
--
--   1. The old `emails` columns (sender_email, receiver_email, sender_user_id,
--      receiver_user_id) are NOT dropped. 220 references across 28 TypeScript
--      files still read them; `email_participants` is the authority and those
--      columns are kept as a denormalized cache of the `from`/first `to`.
--      Dropping them is a separate, later cleanup once those callers move over.
--   2. `customer_id` and `position` are added — the first so a participant can
--      be resolved to a CRM customer, the second to preserve header order.
--   3. `emails.company_id` is added, because participant rows widen who can see
--      a thread and tenancy was previously reconstructed per-query by address
--      matching alone.
--
-- Only ONE of this file and 20260717234741 may ever be applied. Retire that one.

CREATE TABLE email_participants (
    id INT AUTO_INCREMENT PRIMARY KEY,
    email_id INT NOT NULL,
    -- normalized: lowercased bare address, no display name, no angle brackets
    email VARCHAR(320) NOT NULL,
    display_name VARCHAR(255) NULL,
    -- resolved links; NULL when the address is not a known user/customer
    user_id INT NULL,
    customer_id INT NULL,
    type ENUM('from', 'to', 'cc', 'bcc') NOT NULL,
    -- original order within its header, for faithful re-rendering
    position INT NOT NULL DEFAULT 0,
    created_date TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_email_participants_email
        FOREIGN KEY (email_id) REFERENCES emails(id) ON DELETE CASCADE,
    CONSTRAINT fk_email_participants_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE SET NULL,
    CONSTRAINT fk_email_participants_customer
        FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE SET NULL
);

CREATE INDEX idx_email_participants_email_id ON email_participants (email_id);
CREATE INDEX idx_email_participants_user_type ON email_participants (user_id, type);
CREATE INDEX idx_email_participants_email ON email_participants (email);

ALTER TABLE emails ADD COLUMN company_id INT NULL;

ALTER TABLE emails
    ADD CONSTRAINT fk_emails_company
    FOREIGN KEY (company_id) REFERENCES company(id);

-- Backfill company_id, most reliable signal first. Each statement only fills
-- rows still NULL, so precedence is: sender user > receiver user > customer.

UPDATE emails e
JOIN users u ON u.id = e.sender_user_id
SET e.company_id = u.company_id
WHERE e.company_id IS NULL;

UPDATE emails e
JOIN users u ON u.id = e.receiver_user_id
SET e.company_id = u.company_id
WHERE e.company_id IS NULL;

UPDATE emails e
JOIN customers_emails ce
  ON LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1)))
   = LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1)))
JOIN customers c ON c.id = ce.customer_id
SET e.company_id = c.company_id
WHERE e.company_id IS NULL
  AND e.sender_email IS NOT NULL
  AND c.company_id IS NOT NULL;

UPDATE emails e
JOIN customers_emails ce
  ON LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1)))
   = LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1)))
JOIN customers c ON c.id = ce.customer_id
SET e.company_id = c.company_id
WHERE e.company_id IS NULL
  AND e.receiver_email IS NOT NULL
  AND c.company_id IS NOT NULL;

-- Rows still NULL are legacy mail that cannot be attributed to a company.
-- Reads treat NULL as "visible if you are otherwise connected to it", i.e.
-- the behavior before this migration, so nothing vanishes from an inbox.

-- Seed participants from the existing sender/receiver columns, so historical
-- threads have uniform rows and reply-all works on old mail.
INSERT INTO email_participants (email_id, email, user_id, type, position)
SELECT
    e.id,
    LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1))),
    e.sender_user_id,
    'from',
    0
FROM emails e
WHERE e.sender_email IS NOT NULL
  AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.sender_email, '<', -1), '>', 1))) <> '';

INSERT INTO email_participants (email_id, email, user_id, type, position)
SELECT
    e.id,
    LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))),
    e.receiver_user_id,
    'to',
    0
FROM emails e
WHERE e.receiver_email IS NOT NULL
  AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))) <> '';

CREATE INDEX idx_emails_thread ON emails (thread_id);
CREATE INDEX idx_emails_company_thread ON emails (company_id, thread_id);
CREATE INDEX idx_emails_deleted_sent ON emails (deleted_at, sent_at);
