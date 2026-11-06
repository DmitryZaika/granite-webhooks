-- Commissions: rows with no quote (a rep's sheet, a bonus), the day the job
-- was installed (its Installations events all done), and the contract and
-- commission an admin set by hand (they win over the calculated ones).
ALTER TABLE quote_commissions
  MODIFY quote_id INT NULL,
  MODIFY customer_id INT NULL,
  ADD COLUMN customer_label VARCHAR(255) NULL AFTER customer_id,
  ADD COLUMN total_override DECIMAL(12,2) NULL AFTER quote_total,
  ADD COLUMN commission_override DECIMAL(12,2) NULL AFTER commission_amount,
  ADD COLUMN installed_date DATE NULL AFTER paid_date;
