-- Office Manager (position 10) is retired: its people become Scheduling
-- Manager (13) and Inventory Manager (12) in the same company, and the
-- automatic customer texts now go from a Scheduling Manager's line.
INSERT INTO users_positions (user_id, position_id, company_id)
SELECT DISTINCT o.user_id, 13, o.company_id FROM users_positions o
 WHERE o.position_id = 10
   AND NOT EXISTS (SELECT 1 FROM users_positions s
                    WHERE s.user_id = o.user_id AND s.company_id = o.company_id
                      AND s.position_id = 13);
INSERT INTO users_positions (user_id, position_id, company_id)
SELECT DISTINCT o.user_id, 12, o.company_id FROM users_positions o
 WHERE o.position_id = 10
   AND NOT EXISTS (SELECT 1 FROM users_positions s
                    WHERE s.user_id = o.user_id AND s.company_id = o.company_id
                      AND s.position_id = 12);
DELETE FROM users_positions WHERE position_id = 10;
DELETE FROM company_position_sections WHERE position_id = 10;
-- Instructions visible to Office Manager stay visible to its people.
INSERT INTO instruction_access (instruction_id, position_id)
SELECT DISTINCT a.instruction_id, p.position_id FROM instruction_access a
  JOIN (SELECT 12 AS position_id UNION ALL SELECT 13) p
 WHERE a.position_id = 10
   AND NOT EXISTS (SELECT 1 FROM instruction_access b
                    WHERE b.instruction_id = a.instruction_id AND b.position_id = p.position_id);
DELETE FROM instruction_access WHERE position_id = 10;
