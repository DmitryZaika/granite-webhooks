-- Admin › Automations: marking a deal won or a quote sold opens the sale
-- form, filled from the quote. Off until a manager turns it on.
ALTER TABLE company ADD COLUMN auto_sale TINYINT(1) NOT NULL DEFAULT 0;
