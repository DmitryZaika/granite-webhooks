-- A sale belongs to one of the customer's deals: the customer's Projects tab
-- shows each won (or sold) deal as one project with its sales under it.
-- NULL: the sale is not part of a deal.
ALTER TABLE sales
  ADD COLUMN deal_id BIGINT UNSIGNED NULL AFTER customer_id,
  ADD KEY idx_sales_deal (deal_id);
