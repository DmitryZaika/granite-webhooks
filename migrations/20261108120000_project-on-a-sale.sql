-- Admin › Quotes › Automation "Project on a sale": marking a deal Won or a
-- quote Sold starts its project in Projects. Off for every company; a
-- project can always be added by hand.
ALTER TABLE company ADD COLUMN auto_project TINYINT(1) NOT NULL DEFAULT 0;
