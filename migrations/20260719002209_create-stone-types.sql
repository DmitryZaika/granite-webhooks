CREATE TABLE stone_types (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    CONSTRAINT fk_stone_types_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);

INSERT INTO stone_types (name, company_id)
SELECT 'granite', id FROM company;

INSERT INTO stone_types (name, company_id)
SELECT 'quartz', id FROM company;

INSERT INTO stone_types (name, company_id)
SELECT 'marble', id FROM company;

INSERT INTO stone_types (name, company_id)
SELECT 'dolomite', id FROM company;

INSERT INTO stone_types (name, company_id)
SELECT 'quartzite', id FROM company;