-- Admin > Quotes > Automation > Event templates: the catalog extras (Admin >
-- Quotes > Extras, `quote_extras.id`) that tick a template in Mark as Sold
-- when the quote has one of them, e.g. the Plumbing extra ticks the
-- "Plumbing" event. A JSON array of ids; NULL or [] ticks it never.
ALTER TABLE quote_sale_event_templates
    ADD COLUMN trigger_extra_ids JSON NULL AFTER title_text;
