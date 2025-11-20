-- Create the company table
CREATE TABLE IF NOT EXISTS company (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100) NOT NULL,
    address VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    qbo_client_id BLOB,
    qbo_client_secret BLOB
);

-- Insert the initial company
INSERT INTO company (name, address) VALUES ('Default Company', 'Default Address');

CREATE TABLE supports (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100),
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    company_id INT,
    constraint fk_company_supports_id foreign key (company_id) references company(id)
);

CREATE TABLE documents (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100),
    src VARCHAR(255),
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      company_id INT,
    constraint fk_company_documents_id foreign key (company_id) references company(id)
);

CREATE TABLE users (
    id INT PRIMARY KEY AUTO_INCREMENT,
    email VARCHAR(255) NOT NULL,
    password VARCHAR(100),
    name VARCHAR(100),
    phone_number VARCHAR(100),
    is_employee BOOLEAN DEFAULT false,
    is_admin BOOLEAN DEFAULT false,
    is_superuser BOOLEAN DEFAULT false,
    is_deleted BOOLEAN DEFAULT FALSE NOT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    company_id INT,
    telegram_id BIGINT NULL,
    telegram_conf_code INT NULL,
    telegram_conf_expires_at TIMESTAMP NULL,
    temp_telegram_id bigint DEFAULT NULL,
    constraint fk_company_users_id foreign key (company_id) references company(id)
);

create table sessions (
id CHAR (36) primary key,
user_id INT ,
expiration_date TIMESTAMP,
is_deleted BOOLEAN DEFAULT 0,
 created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
 constraint fk_user_id foreign key (user_id) references users(id)
);

CREATE TABLE stones (
    id INT PRIMARY KEY AUTO_INCREMENT,
    type VARCHAR(50),
    name VARCHAR(100),
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      company_id INT,
    length FLOAT NULL,
    width FLOAT NULL,
    amount INT NULL,
    is_display BOOLEAN,
    cost_per_sqft INT,
    retail_price INT,
    on_sale BOOLEAN DEFAULT FALSE,
    level INT,
    finishing VARCHAR(20),
    samples_amount INT DEFAULT 0,
    samples_importance INT,
    constraint fk_company_stones_id foreign key (company_id) references company(id)
);

CREATE TABLE images (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100),
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      company_id INT,
    constraint fk_company_images_id foreign key (company_id) references company(id)
);

CREATE TABLE sinks (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100),
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      company_id INT,
    type VARCHAR(50),
    is_display BOOLEAN NOT NULL DEFAULT 1,
    length FLOAT NULL,
    width FLOAT NULL,
    amount INT NULL,
    retail_price VARCHAR(255),
    cost VARCHAR(255),
    constraint fk_company_sinks_id foreign key (company_id) references company(id)
);

CREATE TABLE suppliers (
    id INT PRIMARY KEY AUTO_INCREMENT,
    website VARCHAR(255),
    supplier_name VARCHAR(255),
    manager VARCHAR(255),
    phone VARCHAR(50),
    email VARCHAR(255),
    notes VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      company_id INT,
    constraint fk_company_suppliers_id foreign key (company_id) references company(id)
);

CREATE TABLE instructions (
    id INT PRIMARY KEY AUTO_INCREMENT,
    title VARCHAR(100),
    parent_id INT,
    after_id INT,
    rich_text TEXT,
    company_id INT,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_parent_id FOREIGN KEY (parent_id) REFERENCES instructions(id)
        ON DELETE SET NULL
        ON UPDATE CASCADE,

    CONSTRAINT fk_after_id FOREIGN KEY (after_id) REFERENCES instructions(id)
        ON DELETE SET NULL
        ON UPDATE CASCADE,
    constraint fk_company_instructions_id foreign key (company_id) references company(id)
);

INSERT INTO users (email, password, is_superuser)
VALUES ('majukita777@gmail.com', '$2a$10$Vr81drnJSvZDqRg1KRyKx.xIKgtx2bOqyvkMlYdzMVgZMVrhNiLj.', 1),
       ('colin99delahunty@gmail.com', '$2a$10$XZuxeQFhrfSimzg9kDCl6.rcTigEEd21PnWiuGxZo7lPRlHkj0B9S', 1);

UPDATE users SET company_id = (SELECT id FROM company WHERE name = 'Default Company' LIMIT 1) WHERE company_id IS NULL;
