-- Which quote email template an email was sent with (NULL = none). Counts
-- per user pick the default template in the Email dialog: the one a user
-- sends with most, else the company's most used.
ALTER TABLE quote_emails
  ADD COLUMN template_id INT NULL,
  ADD KEY idx_quote_emails_template (template_id);
