-- Admin > Schedule: when the installer taps Completed on an installation, the
-- job's sales rep gets a notification that their customer is installed.
-- On unless an admin turns it off.
ALTER TABLE company
    ADD COLUMN notify_rep_on_install_complete TINYINT(1) NOT NULL DEFAULT 1;
