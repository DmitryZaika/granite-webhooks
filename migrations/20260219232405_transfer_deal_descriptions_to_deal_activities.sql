INSERT INTO deal_activities (deal_id, company_id, name, deadline, priority, is_completed)
SELECT
    d.id,
    c.company_id,
    CASE
        WHEN d.description IS NOT NULL AND TRIM(d.description) != ''
            THEN LEFT(TRIM(d.description), 255)
        ELSE 'Follow up'
    END,
    d.due_date,
    'medium',
    0
FROM deals d
JOIN customers c ON d.customer_id = c.id
WHERE d.deleted_at IS NULL
  AND (
    (d.description IS NOT NULL AND TRIM(d.description) != '')
    OR d.due_date IS NOT NULL
  )
  AND d.id NOT IN (
    SELECT DISTINCT deal_id FROM deal_activities WHERE deleted_at IS NULL
  );