INSERT INTO customer_sources (company_id, source_key, label, is_system)
SELECT company.id, src.source_key, src.label, 1
FROM company
CROSS JOIN (
    SELECT 'check-in' AS source_key, 'Walk-in' AS label
    UNION ALL SELECT 'leads', 'Leads'
    UNION ALL SELECT 'call-in', 'Call-in'
    UNION ALL SELECT 'other', 'Other'
) src
WHERE NOT EXISTS (
    SELECT 1
    FROM customer_sources existing
    WHERE existing.company_id = company.id
      AND existing.source_key = src.source_key
      AND existing.deleted_at IS NULL
);
