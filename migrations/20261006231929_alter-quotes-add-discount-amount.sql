-- An exact-amount discount on the whole quote, next to discount_pct. Both
-- apply to the discountable lines; the total discount never exceeds them.
ALTER TABLE quotes
  ADD COLUMN discount_amount DECIMAL(12,2) NOT NULL DEFAULT 0;
