-- Dollars of each extra sold on a quote's taken option (waterfall, miter,
-- phantom_seam, custom_edge, support, faucet, soap_dispenser), worked out by
-- commission sync for the Statistics page's Feature sales widget.
ALTER TABLE quote_commissions ADD COLUMN features_json JSON NULL AFTER parts_json;
