-- AI promises: something the rep sent the customer to approve (a quote, a
-- contract, a layout) is kept too, as kind 'approval'. due_at is a day after
-- it was sent: by then a reply from the customer (a text, an email, a call)
-- closes it, else the rep is reminded to check with them.
ALTER TABLE ai_promises
  MODIFY COLUMN kind ENUM('promise', 'quote', 'approval') NOT NULL DEFAULT 'promise';
