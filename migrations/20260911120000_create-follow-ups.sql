-- Follow-ups for Telnyx calls from phone numbers we cannot match to a customer.
--
-- Each follow-up points at the conversation message row (telnyx_calls) that
-- holds the transcript, so a rep can review the call and then mark it done by
-- setting `marked_at`. Until reviewed, `marked_at` stays NULL; `created_at` is
-- set when the follow-up row is created.
CREATE TABLE IF NOT EXISTS follow_ups (
  id INT AUTO_INCREMENT PRIMARY KEY,
  telnyx_call_id INT NOT NULL,
  created_at DATETIME NULL DEFAULT NULL,
  marked_at DATETIME NULL DEFAULT NULL,
  UNIQUE KEY uniq_follow_ups_telnyx_call (telnyx_call_id),
  CONSTRAINT fk_follow_ups_telnyx_call
    FOREIGN KEY (telnyx_call_id) REFERENCES telnyx_calls(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
