-- A job photo the customer sees on their own page (the crew took it with the
-- job card's Photos button). Photos posted in an event's comments stay inside.
ALTER TABLE event_images
    ADD COLUMN for_customer TINYINT(1) NOT NULL DEFAULT 0;
