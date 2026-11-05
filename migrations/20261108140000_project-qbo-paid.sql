-- When the project's customer was seen fully paid in QuickBooks (the
-- Projects board reads the invoices); cleared when a balance shows again.
-- An installed project that is paid is Paid & Completed.
ALTER TABLE projects ADD COLUMN qbo_paid_at DATETIME NULL;
