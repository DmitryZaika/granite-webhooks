-- Add migration script here
UPDATE deals_list 
SET name = 'Not Contacted Yet' 
WHERE id = 1;