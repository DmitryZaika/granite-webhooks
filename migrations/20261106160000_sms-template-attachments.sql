-- Files (pictures, PDFs, documents) sent with an SMS template:
-- [{"s3_key","content_type","filename","width","height"}], keys under sms-attachments/<company>/.
ALTER TABLE cloudtalk_sms_templates ADD COLUMN attachments JSON NULL;
