INSERT INTO superadmin_companies (user_id, company_id)
SELECT u.id, u.company_id
FROM users u
WHERE u.is_superuser = 1
  AND u.is_deleted = 0
  AND NOT EXISTS (
    SELECT 1 FROM superadmin_companies sc WHERE sc.user_id = u.id
  );
