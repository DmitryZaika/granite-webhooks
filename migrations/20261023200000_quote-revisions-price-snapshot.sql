-- The prices a revision was saved with, so updating the quote's prices later
-- does not change what an old revision shows. NULL on revisions saved before
-- this: they show the quote's current prices.
ALTER TABLE quote_revisions
    ADD COLUMN price_snapshot_json JSON NULL AFTER totals_json;
