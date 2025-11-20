-- Add migration script here
ALTER TABLE slab_inventory DROP COLUMN is_sold;

CREATE TABLE supplier_files (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url VARCHAR(255) NOT NULL,
    supplier_id INTEGER NOT NULL REFERENCES suppliers(id),
    create_date TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE positions (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(255) NOT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE users ADD COLUMN position_id INT REFERENCES positions(id);

INSERT INTO positions (name)
VALUES
('sales_rep'),
('sales_manager'),
('shop_manager'),
('shop_worker'),
('manager'),
('external_marketing'),
('check-in');

CREATE TABLE colors (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    hex_code VARCHAR(8) NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO colors (name, hex_code)
VALUES
('Beige', '#F5F5DC'),
('Black', '#000000'),
('Blue', '#0000FF'),
('Brown', '#A52A2A'),
('Cream', '#F5F5DC'),
('Gold', '#FFD700'),
('Gray', '#808080'),
('Green', '#008000'),
('Orange', '#FFA500'),
('Pink', '#FFC0CB'),
('Purple', '#800080'),
('Red', '#FF0000'),
('Silver', '#C0C0C0'),
('Tan', '#D2B48C'),
('White', '#FFFFFF'),
('Yellow', '#FFFF00');

CREATE TABLE stone_colors (
    id INT AUTO_INCREMENT PRIMARY KEY,
    stone_id INT NOT NULL,
    color_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (stone_id) REFERENCES stones(id) ON DELETE CASCADE,
    FOREIGN KEY (color_id) REFERENCES colors(id) ON DELETE CASCADE
);

CREATE TABLE stone_image_links (
    id INT AUTO_INCREMENT PRIMARY KEY,
    stone_id INT NOT NULL,
    source_stone_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (stone_id) REFERENCES stones(id) ON DELETE CASCADE,
    FOREIGN KEY (source_stone_id) REFERENCES stones(id) ON DELETE CASCADE
);

CREATE TABLE stone_slab_links (
    id INT AUTO_INCREMENT PRIMARY KEY,
    stone_id INT NOT NULL,
    source_stone_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (stone_id) REFERENCES stones(id) ON DELETE CASCADE,
    FOREIGN KEY (source_stone_id) REFERENCES stones(id) ON DELETE CASCADE
);
