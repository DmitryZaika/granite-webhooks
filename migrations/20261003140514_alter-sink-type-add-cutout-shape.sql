-- How a sink is cut into the countertop, set once per sink and shared by every
-- price list and quote. NULL = not set yet (price lists fall back to their own value).
-- cutout_type: 1 drop-in, 2 undermount, 3 farmer
-- cutout_shape: 1 rectangle, 2 oval, 3 double, 4 60/40, 5 40/60, 6 70/30, 7 30/70,
--   8 square corners, 9 rounded corners, 10–19 the doubles 50/50, 60/40, 40/60, 70/30,
--   30/70 in turn with square (even) and rounded (odd) corners
ALTER TABLE sink_type
  ADD COLUMN cutout_type TINYINT UNSIGNED NULL,
  ADD COLUMN cutout_shape TINYINT UNSIGNED NULL,
  ADD COLUMN faucet_hole_count TINYINT UNSIGNED NULL;
