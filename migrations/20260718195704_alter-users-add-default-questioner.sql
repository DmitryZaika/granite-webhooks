ALTER TABLE users
  ADD COLUMN default_questioner_id INT NULL,
  ADD CONSTRAINT fk_users_default_questioner
    FOREIGN KEY (default_questioner_id)
    REFERENCES sales_questionnaires(id)
    ON DELETE SET NULL;