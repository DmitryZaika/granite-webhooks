-- A rep's sheet split some jobs into "(cabinets)" and "(countertops)" rows.
-- When the other half is tied to a CRM quote (same rep, same sold day, same
-- name before the parentheses), tie this row to that quote's customer too, so
-- the salary table shows the job as one customer.
UPDATE quote_commissions s
  JOIN quote_commissions l
    ON l.company_id = s.company_id
   AND l.user_id <=> s.user_id
   AND DATE(l.signed_at) = DATE(s.signed_at)
   AND l.quote_id IS NOT NULL
   AND l.customer_id IS NOT NULL
   AND l.customer_label IS NOT NULL
   AND LOWER(REGEXP_REPLACE(l.customer_label, '[[:space:]]*[(](cabinets|countertops)[)]$', ''))
     = LOWER(REGEXP_REPLACE(s.customer_label, '[[:space:]]*[(](cabinets|countertops)[)]$', ''))
   SET s.customer_id = l.customer_id
 WHERE s.quote_id IS NULL
   AND s.customer_id IS NULL
   AND s.customer_label IS NOT NULL;
