-- The prices a quote is made with: its price list with inventory prices and
-- catalog extras merged in, frozen so later price changes reach the quote only
-- when someone clicks Update prices on it. NULL = not taken yet (the quote's
-- next load takes it).
ALTER TABLE quotes
  ADD COLUMN price_snapshot_json JSON NULL;
