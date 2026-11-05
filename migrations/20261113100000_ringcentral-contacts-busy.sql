-- RingCentral answers "busy" (429) once a rep's sign-in passes its limit,
-- and every request in the next minute or so keeps it busy. The contacts
-- job and customer saves leave that rep alone until contacts_paused_until.
-- ringcentral_user_contacts.pending: the customer changed (or was added)
-- while the rep was busy; the job brings the contact up to date.
ALTER TABLE ringcentral_user_logins ADD COLUMN contacts_paused_until DATETIME NULL;

ALTER TABLE ringcentral_user_contacts ADD COLUMN pending TINYINT(1) NOT NULL DEFAULT 0;
