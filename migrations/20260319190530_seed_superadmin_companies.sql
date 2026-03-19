INSERT INTO superadmin_companies (user_id, company_id)
SELECT u.id, u.company_id
FROM users u
WHERE u.is_superuser = 1
  AND u.is_deleted = 0
  AND u.company_id >= 0
  AND u.id NOT IN (SELECT sc.user_id FROM superadmin_companies sc);