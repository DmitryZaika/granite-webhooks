-- Employee-only notes for the quote's Customer & Address. Never printed.
ALTER TABLE quotes
    ADD COLUMN address_notes TEXT NULL AFTER email;
