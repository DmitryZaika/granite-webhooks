-- AI promises, the rest of the checklist:
-- - a promise checks itself off when a later call, text or email keeps it
--   (closed_by_scan_id is the one that did);
-- - the rep is reminded once at its due time (reminded_at);
-- - an appointment (a visit, measuring, a showroom meeting) is checked
--   against the schedule (is_appointment);
-- - a change the customer asked for in their quote is kept too, as
--   kind 'quote' with the quote to open (quote_id);
-- - the morning list goes out once a day per rep (ai_promise_digests).
ALTER TABLE ai_promises
  ADD COLUMN kind ENUM('promise', 'quote') NOT NULL DEFAULT 'promise' AFTER source,
  ADD COLUMN quote_id INT NULL AFTER customer_email,
  ADD COLUMN is_appointment TINYINT(1) NOT NULL DEFAULT 0 AFTER due_at,
  ADD COLUMN reminded_at DATETIME NULL AFTER closed_at,
  ADD COLUMN closed_by_scan_id BIGINT UNSIGNED NULL AFTER reminded_at,
  ADD KEY idx_ai_promises_reminders (status, reminded_at, due_at);

CREATE TABLE ai_promise_digests (
  user_id INT NOT NULL,
  -- The rep's local day.
  digest_date DATE NOT NULL,
  sent_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, digest_date)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
