-- Add migration script here
ALTER TABLE calendars
    ADD COLUMN slug VARCHAR(50) NULL DEFAULT NULL AFTER color;

UPDATE calendars
SET slug = 'templates'
WHERE deleted_at IS NULL
  AND slug IS NULL
  AND LOWER(TRIM(name)) = 'templates';

UPDATE calendars
SET slug = 'installations'
WHERE deleted_at IS NULL
  AND slug IS NULL
  AND LOWER(TRIM(name)) = 'installations';

UPDATE calendars
SET slug = 'estimates'
WHERE deleted_at IS NULL
  AND slug IS NULL
  AND LOWER(TRIM(name)) = 'estimates';

INSERT INTO calendars (company_id, name, color, slug)
SELECT c.id, 'Templates', 'green', 'templates'
FROM company c
WHERE NOT EXISTS (
    SELECT 1
    FROM calendars cal
    WHERE cal.company_id = c.id
      AND cal.slug = 'templates'
      AND cal.deleted_at IS NULL
);

INSERT INTO calendars (company_id, name, color, slug)
SELECT c.id, 'Installations', 'red', 'installations'
FROM company c
WHERE NOT EXISTS (
    SELECT 1
    FROM calendars cal
    WHERE cal.company_id = c.id
      AND cal.slug = 'installations'
      AND cal.deleted_at IS NULL
);

INSERT INTO calendars (company_id, name, color, slug)
SELECT c.id, 'Estimates', 'blue', 'estimates'
FROM company c
WHERE NOT EXISTS (
    SELECT 1
    FROM calendars cal
    WHERE cal.company_id = c.id
      AND cal.slug = 'estimates'
      AND cal.deleted_at IS NULL
);

CREATE UNIQUE INDEX uk_calendars_company_slug ON calendars (company_id, slug);