-- The event's own phone and email: who to reach for this appointment (a
-- spouse, a homeowner on a contractor's account). NULL uses the customer's.
-- Never copied back to the customer.
ALTER TABLE events
  ADD COLUMN contact_phone VARCHAR(50) NULL,
  ADD COLUMN contact_email VARCHAR(255) NULL;
