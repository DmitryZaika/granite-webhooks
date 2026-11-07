-- A second phone and email for the event (a spouse, a second homeowner).
-- Every automated text and email for the event goes to both.
ALTER TABLE events
  ADD COLUMN contact_phone_2 VARCHAR(50) NULL,
  ADD COLUMN contact_email_2 VARCHAR(255) NULL;
