-- Old sales get the customer's deal they were most likely sold from: a live
-- deal made within a year before the sale (or up to 7 days after it, for a
-- deal entered late), won deals first, then the closest in time. Sales with
-- no such deal stay out of any deal and show as projects of their own.
UPDATE sales s
SET s.deal_id = (
  SELECT d.id FROM deals d
   WHERE d.customer_id = s.customer_id
     AND d.deleted_at IS NULL
     AND d.created_at <= s.sale_date + INTERVAL 7 DAY
     AND d.created_at >= s.sale_date - INTERVAL 365 DAY
   ORDER BY d.is_won = 1 DESC,
            ABS(TIMESTAMPDIFF(SECOND, d.created_at, s.sale_date)),
            d.id DESC
   LIMIT 1
)
WHERE s.deal_id IS NULL AND s.sale_date IS NOT NULL;
