-- Processing state of agentic user messages, maintained by claude-job-runner.
-- NULL = not picked up yet (also the value for every non-agentic or 'ai'
-- row), then 'pending' while a run is in progress, then 'done' or 'failed'.
ALTER TABLE chat_messages
  ADD COLUMN status ENUM('pending', 'done', 'failed') NULL DEFAULT NULL AFTER payload,
  ADD INDEX idx_chat_messages_pickup (sender, is_agentic, status);
