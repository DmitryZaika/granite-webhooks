-- Add migration script here

ALTER TABLE sinks
ADD COLUMN slab_id BIGINT UNSIGNED,
ADD CONSTRAINT fk_sinkslab FOREIGN KEY (slab_id) REFERENCES slab_inventory(id);

UPDATE sinks
SET slab_id = (
    SELECT slab_inventory.id
    FROM slab_inventory
    WHERE slab_inventory.sale_id = sinks.sale_id
    LIMIT 1
)
WHERE sinks.sale_id IS NOT NULL;

CREATE TABLE faucet_type (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url VARCHAR(255) NOT NULL,
    type VARCHAR(255) NOT NULL,
    retail_price DECIMAL(10, 2),
    cost DECIMAL(10, 2),
    is_display BOOLEAN NOT NULL DEFAULT TRUE,
    is_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    supplier_id INT,
    company_id INT NOT NULL,
    CONSTRAINT fk_faucet_supplier_junction
        FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE CASCADE,
    CONSTRAINT fk_faucet_company_junction
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

CREATE TABLE faucets (
    id INT PRIMARY KEY AUTO_INCREMENT,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    sale_id INT,
    faucet_type_id INT NULL,
    price DECIMAL(10, 2),
    is_deleted BOOLEAN NOT NULL DEFAULT FALSE,
    CONSTRAINT fk_faucet_sale_junction
        FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE CASCADE,
    CONSTRAINT fk_faucet_type_junction
        FOREIGN KEY (faucet_type_id) REFERENCES faucet_type(id) ON DELETE CASCADE
);

CREATE TABLE installed_faucets (
    id INT PRIMARY KEY AUTO_INCREMENT,
    url VARCHAR(255) NOT NULL,
    faucet_id INT NOT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_installed_faucet_junction
        FOREIGN KEY (faucet_id) REFERENCES faucet_type(id) ON DELETE CASCADE
);

ALTER TABLE faucets
ADD COLUMN slab_id BIGINT UNSIGNED,
ADD CONSTRAINT fk_faucetslab FOREIGN KEY (slab_id) REFERENCES slab_inventory(id);

CREATE TABLE events (
    id INT AUTO_INCREMENT PRIMARY KEY,
    title VARCHAR(255) NOT NULL,
    description TEXT,
    start_date DATETIME NOT NULL,
    end_date DATETIME NOT NULL,
    all_day BOOLEAN DEFAULT FALSE,
    color VARCHAR(50) DEFAULT 'primary',
    status VARCHAR(50) DEFAULT 'scheduled',
    notes TEXT,
    created_user_id INT NOT NULL,
    assigned_user_id INT DEFAULT NULL,
    sale_id INT DEFAULT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_date TIMESTAMP DEFAULT NULL,

    CONSTRAINT fk_events_created_user
        FOREIGN KEY (created_user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_events_assigned_user
        FOREIGN KEY (assigned_user_id) REFERENCES users(id) ON DELETE SET NULL,
    CONSTRAINT fk_events_sale
        FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE SET NULL
);


create table stripe_payments (
    id binary(16) primary key default (UUID_TO_BIN(uuid())),
    sale_id INT not null,
    stripe_payment_intent_id varchar(255) not null,
    amount_total int not null,
    created_at datetime not null default current_timestamp,
    updated_at datetime not null default current_timestamp on update current_timestamp,
    constraint fk_stripe_payments_sale_id foreign key (sale_id) references sales(id)
);


CREATE TABLE checklists (
    id INT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT NULL,
    installer_id INT NULL,
    customer_name VARCHAR(255) NOT NULL,
    installation_address VARCHAR(255) NOT NULL,
    material_correct BOOLEAN NOT NULL DEFAULT FALSE,
    seams_satisfaction BOOLEAN NOT NULL DEFAULT FALSE,
    appliances_fit BOOLEAN NOT NULL DEFAULT FALSE,
    backsplashes_correct BOOLEAN NOT NULL DEFAULT FALSE,
    edges_correct BOOLEAN NOT NULL DEFAULT FALSE,
    holes_drilled BOOLEAN NOT NULL DEFAULT FALSE,
    cleanup_completed BOOLEAN NOT NULL DEFAULT FALSE,
    comments TEXT NULL,
    signature TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_checklists_customer FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE SET NULL,
    CONSTRAINT fk_checklists_installer FOREIGN KEY (installer_id) REFERENCES users(id) ON DELETE SET NULL
);

ALTER TABLE customers
ADD COLUMN sales_rep INT,
ADD CONSTRAINT fk_customers_sales_rep
FOREIGN KEY (sales_rep) REFERENCES users(id);
