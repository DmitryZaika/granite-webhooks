CREATE TABLE todolist (
    id INT AUTO_INCREMENT PRIMARY KEY,
    rich_text VARCHAR(255) NOT NULL,
    is_done BOOLEAN NOT NULL DEFAULT FALSE,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    user_id INT NOT NULL,
    position INTEGER,
    CONSTRAINT fk_user
      FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE chat_history (
    id INT AUTO_INCREMENT PRIMARY KEY,
    history JSON NOT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    user_id INT NOT NULL,
    CONSTRAINT fk_user_chat_history
      FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE installed_stones (
    id INT PRIMARY KEY AUTO_INCREMENT,
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    stone_id INT,
    constraint fk_installed_stones_id foreign key (stone_id) references stones(id)
);

CREATE TABLE slab_inventory (
    id SERIAL PRIMARY KEY,
    bundle VARCHAR(255),
    stone_id INT REFERENCES stones(id),
    is_sold BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    url VARCHAR(255),
    width VARCHAR(255),
    length VARCHAR(255),
    price DECIMAL(10, 2),
    notes TEXT,
    cut_date DATETIME,
    edge VARCHAR(255),
    room VARCHAR(255),
    backsplash VARCHAR(255),
    tear_out VARCHAR(255),
    ten_year_sealer BOOLEAN,
    waterfall VARCHAR(255),
    corbels INT,
    seam VARCHAR(255),
    stove VARCHAR(255),
    extras JSON NULL,
    room_uuid binary(16),
    square_feet DECIMAL(10,2)
);

ALTER TABLE stones
ADD COLUMN supplier_id INT,
ADD CONSTRAINT fk_stones_supplier
FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE SET NULL;

CREATE TABLE installed_sinks (
    id INT PRIMARY KEY AUTO_INCREMENT,
    url VARCHAR(255),
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    sink_id INT,
    CONSTRAINT fk_installed_sinks_id FOREIGN KEY (sink_id) REFERENCES sinks(id)
);
