-- The price list new quotes get (Admin > Quotes > Price Lists, star). NULL = the
-- first active list by sequence. Kept separate from sequence so picking a
-- default does not reorder the lists.
ALTER TABLE quote_settings
  ADD COLUMN default_price_list_id INT NULL AFTER default_counter_depth;
