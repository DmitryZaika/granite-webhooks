-- Add migration script here
UPDATE deals_list SET deleted_at = CURRENT_TIMESTAMP WHERE id = 4 OR id = 5;