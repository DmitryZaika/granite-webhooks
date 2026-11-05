-- Base pay for everyone with a position (e.g. every Sales Representative), not
-- only one person's: each row has exactly one of user_id and position_id. A
-- person's own base pay replaces their position's.
ALTER TABLE commission_base_pay
  MODIFY user_id INT NULL,
  ADD COLUMN position_id INT NULL AFTER user_id;
