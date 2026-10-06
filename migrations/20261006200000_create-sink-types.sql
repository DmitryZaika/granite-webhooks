CREATE TABLE sink_types (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    CONSTRAINT fk_sink_types_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);

INSERT INTO sink_types (name, company_id)
SELECT 'stainless 18 gauge', id FROM company;

INSERT INTO sink_types (name, company_id)
SELECT 'stainless 16 gauge', id FROM company;

INSERT INTO sink_types (name, company_id)
SELECT 'composite', id FROM company;

INSERT INTO sink_types (name, company_id)
SELECT 'ceramic', id FROM company;

INSERT INTO sink_types (name, company_id)
SELECT 'farm house', id FROM company;

-- Keep any other type a sink already uses, so it stays selectable.
INSERT INTO sink_types (name, company_id)
SELECT MIN(st.type), st.company_id
FROM sink_type st
WHERE TRIM(st.type) <> ''
  AND NOT EXISTS (
    SELECT 1 FROM sink_types t
    WHERE t.company_id = st.company_id AND LOWER(t.name) = LOWER(st.type)
  )
GROUP BY st.company_id, LOWER(st.type);
