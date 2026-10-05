-- Deterministic test data for the `api` Lambda.
--
-- Used two ways:
--   1. Integration tests: `#[sqlx::test(fixtures(path = "../seed", scripts("local_seed")))]`
--      loads it into a fresh, migrated database per test.
--   2. Local runs: `make api-db-seed` loads it into the docker MySQL on :3307.
--
-- All ids are >= 100 so they never collide with rows the migrations insert.
-- Every user's password is `granite-local` (bcrypt below), for logging into the
-- Remix app locally. Session ids are fixed so tests and curl can sign cookies.
--
-- When a migrated route needs more data, append to this file; do not edit rows
-- existing tests assert on.

INSERT INTO company (id, name) VALUES
  (100, 'Acme Stone'),
  (101, 'Other Granite Co');

INSERT INTO users (id, email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id) VALUES
  (100, 'rep@acme.test',        '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Rita Rep',       1, 0, 0, 0, 100),
  (101, 'manager@acme.test',    '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Mark Manager',   1, 0, 0, 0, 100),
  (102, 'admin@acme.test',      '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Ada Admin',      0, 1, 0, 0, 100),
  (103, 'super@other.test',     '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Sam Super',      0, 0, 0, 0, 101),
  (104, 'viewer@acme.test',     '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Vic Viewer',     0, 0, 0, 0, 100),
  (105, 'gone@acme.test',       '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Gus Gone',       1, 0, 0, 1, 100),
  (106, 'rep@other.test',       '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Olga Other',     1, 0, 0, 0, 101);

-- position ids: 1 sales_rep, 2 sales_manager, 9 super_admin
INSERT INTO users_positions (user_id, position_id, company_id) VALUES
  (100, 1, 100),
  (101, 2, 100),
  (101, 1, 101),  -- manager in 100 only; a position elsewhere must not leak
  (103, 9, 100),
  (103, 9, 101),
  (106, 1, 101);

-- Session ids: aaaaaaaa-0000-4000-8000-000000000<user id>, plus broken ones.
INSERT INTO sessions (id, user_id, expiration_date, is_deleted, created_date) VALUES
  ('aaaaaaaa-0000-4000-8000-000000000100', 100, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000101', 101, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000102', 102, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000103', 103, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000104', 104, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000105', 105, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('aaaaaaaa-0000-4000-8000-000000000106', 106, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP),
  ('bbbbbbbb-0000-4000-8000-0000000e0100', 100, '2020-01-01 00:00:00', 0, '2019-12-01 00:00:00'),                   -- expired
  ('bbbbbbbb-0000-4000-8000-0000000d0100', 100, '2037-12-31 00:00:00', 1, CURRENT_TIMESTAMP),                       -- logged out
  ('bbbbbbbb-0000-4000-8000-0000000a0100', 100, '2037-12-31 00:00:00', 0, DATE_SUB(CURRENT_TIMESTAMP, INTERVAL 3 MONTH)); -- too old

-- Customers of company 100. Ids 1000+; 990000021 is the onboarding sample id.
INSERT INTO customers (id, name, phone, phone_2, address, company_id, source, sales_rep, created_date, assigned_date, invalid_lead, deleted_at, parent_id, company_name, customer_temperature) VALUES
  (1000, 'Alice Lead',             '317-555-0100', '317-555-0101', '1 Main St',  100, 'leads',      100, '2026-01-15 10:00:00', '2026-01-16 12:30:00', NULL,        NULL,                  NULL, NULL,                'hot'),
  (1001, 'Bob Unassigned',         '317-555-0102', NULL,           NULL,         100, 'check-in',   NULL,'2026-02-01 09:00:00', NULL,                  NULL,        NULL,                  NULL, NULL,                NULL),
  (1002, 'Carol Invalid',          '317-555-0103', '',             '3 Elm St',   100, 'leads',      100, '2026-02-02 09:00:00', '2026-02-02 10:00:00', 'spam',      NULL,                  NULL, NULL,                'cold'),
  (1003, 'Dan Deleted',            '317-555-0104', NULL,           NULL,         100, 'leads',      100, '2026-02-03 09:00:00', NULL,                  NULL,        '2026-03-01 00:00:00', NULL, NULL,                NULL),
  (1004, 'Erin Builder',           '317-555-0105', NULL,           '5 Oak Ave',  100, 'call-in',    101, '2026-03-10 23:59:59', NULL,                  NULL,        NULL,                  NULL, 'Erin Builders LLC', 'medium'),
  (1005, 'Frank Child',            NULL,           NULL,           NULL,         100, 'other',      101, '2026-03-11 08:00:00', NULL,                  NULL,        NULL,                  1004, NULL,                NULL),
  (1006, 'Gina Deleted Rep',       '317-555-0107', NULL,           NULL,         100, 'leads',      105, '2026-03-12 08:00:00', '2026-03-12 08:05:00', NULL,        NULL,                  NULL, NULL,                NULL),
  (1007, 'Hank Invalid Company',   NULL,           NULL,           NULL,         100, 'user-input', NULL,'2026-03-13 08:00:00', NULL,                  'duplicate', NULL,                  NULL, 'Hank Co',           NULL),
  (1008, 'Ivy Empty Strings',      '',             NULL,           '',           100, 'other',      100, '2026-03-14 08:00:00', NULL,                  '',          NULL,                  NULL, '',                  NULL),
  (1009, 'Onboarding Sample Customer', NULL,       NULL,           NULL,         100, 'leads',      NULL,'2026-03-15 08:00:00', NULL,                  NULL,        NULL,                  NULL, NULL,                NULL),
  (1010, 'Zoë Ünicode-Łukasz',     '317-555-0110', NULL,           'Straße 7',   100, 'leads',      101, '2026-03-16 08:00:00', '2026-03-16 09:00:00', NULL,        NULL,                  NULL, NULL,                NULL),
  (1011, 'Jack Mismatched Email',  NULL,           NULL,           NULL,         100, 'other',      100, '2026-03-17 08:00:00', NULL,                  NULL,        NULL,                  NULL, NULL,                NULL),
  (990000021, 'Sample By Id',      NULL,           NULL,           NULL,         100, 'leads',      NULL,'2026-03-18 08:00:00', NULL,                  NULL,        NULL,                  NULL, NULL,                NULL),
  -- company 101: must never be visible to company 100 users
  (2000, 'Other Co Customer',      '463-555-0200', NULL,           NULL,         101, 'leads',      106, '2026-04-01 08:00:00', '2026-04-01 08:00:00', NULL,        NULL,                  NULL, 'Other Co Corp',     NULL);

-- Email ids 5000+. Alice's primary (5001) was added after 5000 so ordering
-- must put the primary first, then by created_at.
INSERT INTO customers_emails (id, customer_id, email, created_at, created_by) VALUES
  (5000, 1000, 'alice.old@example.com',   '2026-01-15 10:00:00', 100),
  (5001, 1000, 'alice@example.com',       '2026-01-15 11:00:00', 100),
  (5002, 1000, 'alice.work@example.com',  '2026-01-15 09:00:00', 100),
  (5003, 1004, 'erin@builders.example',   '2026-03-10 10:00:00', 101),
  (5004, 1010, 'zoe@example.com',         '2026-03-16 08:00:00', 101),
  (5005, 1011, 'jack@example.com',        '2026-03-17 08:00:00', 100),
  (5006, 2000, 'other@otherco.example',   '2026-04-01 08:00:00', 106),
  (5007, 1002, 'carol@example.com',       '2026-02-02 09:00:00', 100);

UPDATE customers SET email_id = 5001 WHERE id = 1000;
UPDATE customers SET email_id = 5003 WHERE id = 1004;
UPDATE customers SET email_id = 5004 WHERE id = 1010;
UPDATE customers SET email_id = 5000 WHERE id = 1011;  -- points at Alice's row: no primary email
UPDATE customers SET email_id = 5006 WHERE id = 2000;
UPDATE customers SET email_id = 5007 WHERE id = 1002;

-- Erin's company: 2 direct sales + 1 via child customer 1005.
INSERT INTO sales (id, customer_id, price, status, project_address) VALUES
  (7000, 1004, 1000.50, 'sold', '5 Oak Ave'),
  (7001, 1004, 2000.00, 'sold', '5 Oak Ave'),
  (7002, 1005, 500.25,  'sold', '6 Oak Ave'),
  (7003, 2000, 9999.99, 'sold', 'elsewhere');
