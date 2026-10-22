-- Name the customer chose to leave on the survey; NULL keeps it anonymous.
ALTER TABLE customer_surveys ADD COLUMN customer_name VARCHAR(255) NULL;
