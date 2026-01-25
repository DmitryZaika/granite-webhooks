-- Add migration script here

CREATE TABLE notifications (
    id SERIAL PRIMARY KEY,
    user_id INT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    customer_id INT REFERENCES customers(id) ON DELETE SET NULL,
    message VARCHAR(255) NOT NULL,
    due_at TIMESTAMP NOT NULL,
    is_done TINYINT DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE deals_list (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    position INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL
);

INSERT INTO deals_list (id, name, position)
VALUES
  (1, 'New Customers', 0),
  (2, 'Contacted',     1),
  (3, 'Got a Quote',   2),
  (4, 'Closed Won',     3),
  (5, 'Closed Lost',    4);

  CREATE TABLE deals (
      id SERIAL PRIMARY KEY,
      customer_id INT NOT NULL,
      amount DECIMAL(10, 2) NULL,
      description TEXT NULL,
      status VARCHAR(255) NULL,
      list_id INT NOT NULL,
      position INT NOT NULL,
      created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      deleted_at DATETIME NULL,
      user_id INT NULL,
      lost_reason VARCHAR(255) NULL,
      updated_at TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP,
      due_date DATE,
      FOREIGN KEY (customer_id) REFERENCES customers(id),
      FOREIGN KEY (list_id) REFERENCES deals_list(id),
      FOREIGN KEY (user_id) REFERENCES users(id)
  );

  INSERT INTO deals (customer_id, list_id, position, user_id)
  SELECT
    c.id           AS customer_id,
    1              AS list_id,
    0              AS position,
    c.sales_rep      AS user_id
  FROM customers c
  WHERE c.sales_rep IS NOT NULL
    AND NOT EXISTS (
      SELECT 1 FROM deals d
      WHERE d.customer_id = c.id
    );

UPDATE customers
SET source = "check-in"
WHERE from_check_in = 1;

ALTER TABLE customers DROP COLUMN from_check_in;

UPDATE deals_list SET position = position + 1 WHERE deleted_at IS NULL AND position >= 3;

INSERT INTO deals_list (name, position) VALUES ('On Hold', 3);

ALTER TABLE customers ADD COLUMN assigned_date TIMESTAMP DEFAULT NULL;

CREATE TABLE users_positions (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    position_id INT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id),
    FOREIGN KEY (position_id) REFERENCES positions(id)
);

CREATE TABLE deals_images (
    id INT AUTO_INCREMENT PRIMARY KEY,
    deal_id BIGINT UNSIGNED NOT NULL,
    image_url VARCHAR(255) NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (deal_id) REFERENCES deals(id) ON DELETE CASCADE
);

UPDATE deals d
JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
SET d.status = dl.name
WHERE d.deleted_at IS NULL;
