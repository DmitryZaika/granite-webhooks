-- Customers in each rep's own RingCentral contacts. RingCentral contacts
-- belong to one login, so the company's connection fills only the address
-- book of whoever connected it (ringcentral_contacts); every rep who
-- connected their own RingCentral (ringcentral_user_logins) gets each
-- customer too (common::telephony::ringcentral_contacts).
--
-- ringcentral_user_contacts: one row per rep and customer. ringcentral_id is
-- the contact in that rep's book, NULL when RingCentral refused it
-- (last_error says why; the customer's next save tries again).
-- ringcentral_user_logins.contacts_indexed_at: when the rep's book was read
-- and its contacts matched to customers by phone, so nobody gets a second
-- copy of a contact they already had. A new sign-in reads it again.
CREATE TABLE ringcentral_user_contacts (
  user_id INT NOT NULL,
  customer_id INT NOT NULL,
  ringcentral_id BIGINT NULL,
  last_error VARCHAR(500) NULL,
  synced_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, customer_id),
  KEY idx_ringcentral_user_contacts_customer (customer_id)
);

ALTER TABLE ringcentral_user_logins ADD COLUMN contacts_indexed_at DATETIME NULL;
