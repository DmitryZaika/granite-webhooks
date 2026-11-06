-- Employee CRM › Automations: whose automated emails and texts go out when a
-- deal of this employee lands on a list (e.g. Not Contacted Yet).
--   'company': the company's automated templates (Admin › Automations), as
--              before. Every user starts here.
--   'own':     only the employee's own automated templates; the company's
--              do not go to their deals.
ALTER TABLE users
  ADD COLUMN lead_automation ENUM('company', 'own') NOT NULL DEFAULT 'company';
