ALTER TABLE sinks
ADD COLUMN supplier_id INT,
ADD CONSTRAINT fk_supplier
FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE SET NULL;

CREATE TABLE customers (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255),
    email VARCHAR(255),
    phone VARCHAR(50),
    address VARCHAR(255),
    city VARCHAR(100),
    state VARCHAR(100),
    postal_code VARCHAR(20),
    notes TEXT,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    company_id INT,
    view_id binary(16) NOT NULL DEFAULT (UUID_TO_BIN(uuid())),
    qbo_id INT,
    company_name VARCHAR(255) NULL,
    referral_source VARCHAR(255) NULL,
    from_check_in TINYINT(1) NULL,
    source VARCHAR(255) NULL,
    remodal_type VARCHAR(255) NULL,
    project_size VARCHAR(255) NULL,
    contact_time VARCHAR(255) NULL,
    remove_and_dispose VARCHAR(255) NULL,
    improve_offer TINYINT(1) NULL,
    sink VARCHAR(255) NULL,
    when_start VARCHAR(255) NULL,
    details TEXT NULL,
    compaign_name VARCHAR(255) NULL,
    adset_name VARCHAR(255) NULL,
    ad_name VARCHAR(255) NULL,
    backsplash VARCHAR(255) NULL,
    kitchen_stove VARCHAR(255) NULL,
    your_message TEXT NULL,
    attached_file VARCHAR(255) NULL,
    invalid_lead varchar(255) DEFAULT NULL,
    deleted_at DATETIME NULL DEFAULT NULL,
    CONSTRAINT fk_company_customers
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

CREATE TABLE sales (
    id INT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT,
    seller_id INT,
    price DECIMAL(10, 2),
    company_id INT,
    status VARCHAR(255) NOT NULL,
    cancelled_date DATETIME NULL,
    installed_date DATETIME NULL,
    notes TEXT DEFAULT NULL,
    square_feet DECIMAL(10,2) DEFAULT NULL,
    project_address VARCHAR(255) NOT NULL,
    qbo_id INT,
    extras JSON,

    sale_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_customer_sales
        FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE,
    CONSTRAINT fk_seller_sales
        FOREIGN KEY (seller_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_company_sales
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

ALTER TABLE slab_inventory
ADD COLUMN sale_id INT NULL,
ADD COLUMN parent_id BIGINT UNSIGNED,
ADD CONSTRAINT fk_sale_slab_junction_2
    FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE CASCADE,
ADD CONSTRAINT fk_parent_slab_junction
    FOREIGN KEY (parent_id) REFERENCES slab_inventory(id) ON DELETE CASCADE;

CREATE TABLE sink_type (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url VARCHAR(255) NOT NULL,
    type VARCHAR(255) NOT NULL,
    retail_price DECIMAL(10, 2),
    cost DECIMAL(10, 2),
    is_display BOOLEAN NOT NULL DEFAULT TRUE,
    is_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    length INT NOT NULL,
    width INT NOT NULL,
    depth INT NOT NULL,
    supplier_id INT,
    company_id INT NOT NULL,
    CONSTRAINT fk_supplier_junction
        FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE CASCADE,
    CONSTRAINT fk_company_junction
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

ALTER TABLE sinks
    ADD COLUMN sale_id INT,
    ADD COLUMN sink_type_id INT NULL,
    ADD COLUMN price DECIMAL(10, 2),
    ADD COLUMN is_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    DROP COLUMN name,
    DROP COLUMN url,
    DROP COLUMN type,
    DROP COLUMN is_display,
    DROP COLUMN length,
    DROP COLUMN width,
    DROP COLUMN retail_price,
    DROP COLUMN cost,
    DROP FOREIGN KEY fk_supplier,
    DROP COLUMN supplier_id,
    DROP COLUMN amount,
    DROP FOREIGN KEY fk_company_sinks_id,
    DROP COLUMN company_id,
    ADD CONSTRAINT fk_sink_sale_junction_2
            FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE CASCADE,
    ADD CONSTRAINT fk_sink_junction
            FOREIGN KEY (sink_type_id) REFERENCES sink_type(id) ON DELETE CASCADE
