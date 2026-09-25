-- Optional sales rating on a customer. NULL means the user has not selected one.
ALTER TABLE customers
  ADD COLUMN customer_temperature ENUM('hot', 'medium', 'cold') NULL DEFAULT NULL;
