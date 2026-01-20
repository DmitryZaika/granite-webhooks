-- Добавляем группу только если её еще нет
INSERT INTO groups_list (name, company_id) 
SELECT 'Default', 0 
FROM (SELECT 1) AS tmp
WHERE NOT EXISTS (SELECT 1 FROM groups_list WHERE name = 'Default' AND company_id = 0);

UPDATE deals_list 
SET group_id = (SELECT id FROM groups_list WHERE name = 'Default' AND company_id = 0 LIMIT 1)
WHERE group_id IS NULL;

ALTER TABLE deals_list DROP FOREIGN KEY fk_deals_list_group_id;

ALTER TABLE deals_list MODIFY COLUMN group_id INT NOT NULL;

ALTER TABLE deals_list ADD CONSTRAINT fk_deals_list_group_id FOREIGN KEY (group_id) REFERENCES groups_list(id);
