-- Admin > Quotes > Automation > Event templates: single options of catalog
-- extras that tick a template in Mark as Sold when the quote has a line with
-- that option, e.g. only the "Undermount sink" option of the Sink extra. A
-- JSON array of {"extra_id": quote_extras.id, "option_id": arrOptions[].id};
-- NULL or [] ticks it by option never (trigger_extra_ids still applies).
ALTER TABLE quote_sale_event_templates
    ADD COLUMN trigger_options JSON NULL AFTER trigger_extra_ids;
