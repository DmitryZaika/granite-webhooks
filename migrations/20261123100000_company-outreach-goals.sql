-- The minimum calls, texts and emails each rep should make a day, set by a
-- Manager or Sales Manager and shown on the dashboard's Outreach activity widget.
ALTER TABLE company
  ADD COLUMN daily_calls_goal SMALLINT UNSIGNED NULL,
  ADD COLUMN daily_sms_goal SMALLINT UNSIGNED NULL,
  ADD COLUMN daily_emails_goal SMALLINT UNSIGNED NULL;
