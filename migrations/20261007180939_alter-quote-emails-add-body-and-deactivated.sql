-- body_html: the message as it was sent (shown when the subject is clicked on
-- the quote page). deactivated_at: the email's quote link was switched off; the
-- customer portal no longer opens with it. NULL = active.
ALTER TABLE quote_emails
  ADD COLUMN body_html MEDIUMTEXT NULL,
  ADD COLUMN deactivated_at DATETIME NULL;
