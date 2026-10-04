-- referral_source exactly as the webhook sent it, when it was rewritten before saving.
ALTER TABLE customers ADD COLUMN referral_source_raw VARCHAR(255) NULL AFTER referral_source;
