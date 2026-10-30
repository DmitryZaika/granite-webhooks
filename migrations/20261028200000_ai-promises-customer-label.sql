-- AI promises: the name the customer goes by in the call or message
-- ("Donna"), kept for a caller whose number is not on any customer, so a
-- later email or text to them can still close the promise and the list can
-- name them.
ALTER TABLE ai_promises
  ADD COLUMN customer_label VARCHAR(120) NULL AFTER customer_email;
