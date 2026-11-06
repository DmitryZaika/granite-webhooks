-- Granite Phone contact groups: groups the company's users make in the
-- app's Contacts (like the built-in Coworkers), e.g. "Installers" or
-- "Builders". Groups are the company's: every user of the company sees
-- each group and its members, and anyone may create, rename or delete one
-- and change who is in it. Hiding a group from the top of Contacts is each
-- user's own.
--
-- phone_contact_groups: one row per group; created_by is who made it (NULL
--   once that user is gone).
-- phone_contact_group_members: who is in a group, each row a customer
--   (customer_id) or a coworker (user_id), exactly one of the two (the API
--   checks it: MySQL refuses CHECK constraints on columns with foreign key
--   actions). Deleted customers and users stay listed here but the API
--   leaves them out.
-- phone_contact_group_hidden: the groups a user hid from the top of their
--   Contacts, each a group (group_id) or the built-in Coworkers (builtin =
--   'coworkers'), exactly one of the two. A hidden group stays intact and
--   comes back from Contacts' "Groups".
--
-- All dates are UTC. No triggers, views or generated columns: test-db
-- copies tables only.

SET SESSION lock_wait_timeout = 3;

CREATE TABLE IF NOT EXISTS phone_contact_groups (
  id         BIGINT      NOT NULL AUTO_INCREMENT,
  company_id INT         NOT NULL,
  -- 1 to 60 characters, unique in the company (case-insensitive)
  name       VARCHAR(60) NOT NULL,
  created_by INT         NULL,
  created_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP
                         ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  KEY idx_phone_contact_groups_company (company_id, name),
  CONSTRAINT fk_phone_contact_groups_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_phone_contact_groups_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_contact_group_members (
  id          BIGINT   NOT NULL AUTO_INCREMENT,
  group_id    BIGINT   NOT NULL,
  customer_id INT      NULL,
  user_id     INT      NULL,
  added_by    INT      NULL,
  created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_contact_group_members_customer (group_id, customer_id),
  UNIQUE KEY uq_phone_contact_group_members_user (group_id, user_id),
  KEY idx_phone_contact_group_members_customer (customer_id),
  KEY idx_phone_contact_group_members_user (user_id),
  CONSTRAINT fk_phone_contact_group_members_group FOREIGN KEY (group_id) REFERENCES phone_contact_groups (id) ON DELETE CASCADE,
  CONSTRAINT fk_phone_contact_group_members_customer FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE,
  CONSTRAINT fk_phone_contact_group_members_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_contact_group_hidden (
  id         BIGINT   NOT NULL AUTO_INCREMENT,
  user_id    INT      NOT NULL,
  group_id   BIGINT   NULL,
  builtin    ENUM('coworkers') NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_contact_group_hidden_group (user_id, group_id),
  UNIQUE KEY uq_phone_contact_group_hidden_builtin (user_id, builtin),
  KEY idx_phone_contact_group_hidden_group (group_id),
  CONSTRAINT fk_phone_contact_group_hidden_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
  CONSTRAINT fk_phone_contact_group_hidden_group FOREIGN KEY (group_id) REFERENCES phone_contact_groups (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
