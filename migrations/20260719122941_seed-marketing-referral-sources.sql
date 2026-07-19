INSERT INTO marketing_refferal_sources (company_id, name)
SELECT company.id, source.name
FROM company
CROSS JOIN (
    SELECT 'missed call' AS name
    UNION ALL SELECT 'email'
    UNION ALL SELECT 'website seo'
    UNION ALL SELECT 'website ads'
    UNION ALL SELECT 'website social'
    UNION ALL SELECT 'marketplace'
    UNION ALL SELECT 'google'
    UNION ALL SELECT 'facebook'
    UNION ALL SELECT 'instagram'
    UNION ALL SELECT 'LSA'
) source
WHERE NOT EXISTS (
    SELECT 1
    FROM marketing_refferal_sources existing
    WHERE existing.company_id = company.id
      AND LOWER(existing.name) = LOWER(source.name)
      AND existing.deleted_at IS NULL
);
