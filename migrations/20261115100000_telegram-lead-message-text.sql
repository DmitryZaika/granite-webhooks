-- The text each Telegram lead message was sent with, so an assignment made in
-- the app can edit the managers' messages to "Lead assigned to …" like the
-- Telegram buttons do (the Bot API cannot read a sent message back).
ALTER TABLE telegram_lead_messages ADD COLUMN text TEXT NULL AFTER message_id;
