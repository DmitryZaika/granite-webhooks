-- The owner asked (2026-11-04) that no commission count as paid to the rep for
-- now: they send the reps' sheets and the paid ones are marked again by hand.
-- Payroll dates stay.
UPDATE quote_commissions SET paid_out_at = NULL WHERE paid_out_at IS NOT NULL;
