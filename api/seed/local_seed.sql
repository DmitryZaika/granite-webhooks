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

-- Sales reps (position 1). A deleted user holding the position must not be
-- listed; 101 holds it only in company 101 but is listed in 100 (Remix parity:
-- `up.company_id` is not filtered).
INSERT INTO users_positions (user_id, position_id, company_id) VALUES
  (105, 1, 100);

-- ---- Inventory: stones, slabs, sinks, faucets (GET /v1/stones, /v1/sinks, /v1/faucets)
-- Ids: suppliers 300+, stones 3000+, sink types 3100+, faucet types 3200+,
-- slabs 6000+, sink units 6100+, faucet units 6200+. Colors 1..16 come from the
-- migrations (2 = Black, 15 = White).

INSERT INTO suppliers (id, supplier_name, company_id) VALUES
  (300, 'Stone Source Inc', 100),
  (301, 'Quartz Partners',  100),
  (302, 'Other Co Supplier', 101);

INSERT INTO stones (id, name, type, url, company_id, supplier_id, is_display, on_sale, regular_stock, length, width, retail_price, cost_per_sqft, level, finishing, samples_amount, samples_importance, bundle_number, bundle_location, delivery_date, created_date, deleted_at) VALUES
  -- 3 whole slabs free, 1 sold, 1 cut, 1 deleted: total 4, available 3, whole 4/3
  (3000, 'Absolute Black',          'granite',   'https://img.example/absolute-black.jpg', 100, 300, 1, 1, 0, 126.5, 63, 75, 40, 2, 'polished', 3, 1, 'B-12', 'Rack 3', '2026-05-01', '2026-01-10 08:00:00', NULL),
  -- whole slab sold, its remnant is free: "Remnants Only"; price by sqft
  (3001, 'Calacatta Gold',          'quartz',    'https://img.example/calacatta.jpg',      100, 301, 1, 0, 0, 120,   55, 0,  55, 5, 'honed',    0, NULL, NULL, NULL, NULL,       '2026-02-11 09:30:00', NULL),
  -- no slabs, not regular stock: hidden unless sold out stones are shown
  (3002, 'Sold Out Marble',         'marble',    'https://img.example/sold-out.jpg',       100, 300, 1, 0, 0, NULL,  NULL, 90, 50, 3, 'polished', 0, NULL, '0',  '',     NULL,       '2026-02-12 09:30:00', NULL),
  -- regular stock with no slabs: always listed
  (3003, 'Regular Stock Quartzite', 'quartzite', 'https://img.example/regular.jpg',        100, 300, 1, 0, 1, 130,   65, 110, 70, NULL, NULL,     0, NULL, NULL, NULL, NULL,       '2026-02-13 09:30:00', NULL),
  -- hidden from employees (is_display = 0)
  (3004, 'Hidden Granite',          'granite',   'https://img.example/hidden.jpg',         100, 300, 0, 0, 0, 100,   50, 60, 30, 2, 'polished', 0, NULL, NULL, NULL, NULL,       '2026-02-14 09:30:00', NULL),
  -- soft-deleted
  (3005, 'Deleted Granite',         'granite',   'https://img.example/deleted.jpg',        100, 300, 1, 0, 0, 100,   50, 60, 30, 2, 'polished', 0, NULL, NULL, NULL, NULL,       '2026-02-15 09:30:00', '2026-03-01 00:00:00'),
  -- no image: the API lists it, the employee page does not render it
  (3006, 'No Image Granite',        'granite',   NULL,                                     100, 300, 1, 0, 0, 100,   50, 65, 35, 1, 'leathered',0, NULL, NULL, NULL, NULL,       '2026-02-16 09:30:00', NULL),
  -- draws slabs from Absolute Black through stone_slab_links
  (3007, 'Linked Black Remnants',   'granite',   'https://img.example/linked.jpg',         100, 300, 1, 0, 0, NULL,  NULL, 70, 38, 2, 'polished', 0, NULL, NULL, NULL, NULL,       '2026-02-17 09:30:00', NULL),
  -- leathered quartz, Black + White colors, 1 free slab
  (3008, 'Zebra Quartz',            'quartz',    'https://img.example/zebra.jpg',          100, 301, 1, 0, 0, 118,   56, 95, 52, 4, 'leathered',0, NULL, NULL, NULL, NULL,       '2026-02-18 09:30:00', NULL),
  -- the onboarding tour's sample name: the page never lists a real stone with it
  (3009, 'Onboarding Sample Granite','granite',  'https://img.example/onboarding.jpg',     100, 300, 1, 0, 0, 120,   60, 80, 40, 2, 'polished', 0, NULL, NULL, NULL, NULL,       '2026-02-19 09:30:00', NULL),
  -- company 101
  (4000, 'Other Co Granite',        'granite',   'https://img.example/other.jpg',          101, 302, 1, 0, 0, 100,   50, 60, 30, 2, 'polished', 0, NULL, NULL, NULL, NULL,       '2026-02-20 09:30:00', NULL);

INSERT INTO stone_slab_links (stone_id, source_stone_id) VALUES
  (3007, 3000);

INSERT INTO stone_colors (stone_id, color_id) VALUES
  (3000, 2),
  (3008, 2),
  (3008, 15),
  (3001, 15);

INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, parent_id, cut_date, deleted_at) VALUES
  (6000, 3000, 'B-12-1', NULL, NULL, NULL,                  NULL),
  (6001, 3000, 'B-12-2', NULL, NULL, NULL,                  NULL),
  (6002, 3000, 'B-12-3', NULL, NULL, NULL,                  NULL),
  (6003, 3000, 'B-12-4', 7000, NULL, NULL,                  NULL),
  (6004, 3000, 'B-12-5', 7000, NULL, '2026-04-01 10:00:00', NULL),
  (6005, 3000, 'B-12-6', NULL, NULL, NULL,                  '2026-04-02 10:00:00'),
  (6010, 3001, 'CG-1',   7001, NULL, NULL,                  NULL),
  (6011, 3001, 'CG-1a',  NULL, 6010, NULL,                  NULL),
  (6020, 3004, 'H-1',    NULL, NULL, NULL,                  NULL),
  (6030, 3006, 'NI-1',   NULL, NULL, NULL,                  NULL),
  (6040, 3008, 'Z-1',    NULL, NULL, NULL,                  NULL),
  (6050, 3009, 'OS-1',   NULL, NULL, NULL,                  NULL),
  (6060, 4000, 'O-1',    NULL, NULL, NULL,                  NULL);

INSERT INTO sink_type (id, name, url, type, retail_price, cost, is_display, is_deleted, length, width, depth, supplier_id, company_id, regular_stock) VALUES
  -- 2 free units, 1 installed on a slab, 1 deleted: 2 available
  (3100, 'Undermount 3219',    'https://img.example/undermount.jpg', 'stainless 18 gauge', 199.00, 90.00,  1, 0, 32, 19, 9,  300, 100, 0),
  -- regular stock, no units, price 0: "On-Demand", "Contact for price"
  (3101, 'Farmhouse 33',       'https://img.example/farmhouse.jpg',  'farm house',         0.00,   NULL,   1, 0, 33, 22, 10, 301, 100, 1),
  -- not on display
  (3102, 'Composite Black',    'https://img.example/composite.jpg',  'composite',          249.50, 120.00, 0, 0, 30, 18, 9,  300, 100, 0),
  -- no units, not regular stock: out of stock
  (3103, 'Ceramic Out',        'https://img.example/ceramic.jpg',    'ceramic',            NULL,   NULL,   1, 0, 0,  0,  0,  300, 100, 0),
  -- deleted
  (3104, 'Deleted Sink',       'https://img.example/deleted.jpg',    'composite',          100.00, 50.00,  1, 1, 30, 18, 9,  300, 100, 0),
  (3105, 'Dual Bowl 16',       'https://img.example/dual.jpg',       'stainless 16 gauge', 329.00, 150.00, 1, 0, 33, 22, 9,  301, 100, 0),
  -- type outside the known list sorts after the known ones
  (3107, 'Bar Prep Sink',      'https://img.example/bar.jpg',        'bar sink',           149.00, 70.00,  1, 0, 15, 15, 7,  300, 100, 0),
  -- company 101
  (3106, 'Other Co Sink',      'https://img.example/other-sink.jpg', 'composite',          100.00, 50.00,  1, 0, 30, 18, 9,  302, 101, 0);

INSERT INTO sinks (id, sink_type_id, is_deleted, slab_id) VALUES
  (6100, 3100, 0, NULL),
  (6101, 3100, 0, NULL),
  (6102, 3100, 0, 6003),
  (6103, 3100, 1, NULL),
  (6104, 3102, 0, NULL),
  (6105, 3104, 0, NULL),
  (6106, 3105, 0, NULL),
  (6107, 3107, 0, NULL),
  (6108, 3106, 0, NULL);

INSERT INTO faucet_type (id, name, url, type, retail_price, cost, is_display, is_deleted, supplier_id, company_id, regular_stock) VALUES
  -- 2 free units, 1 on a slab, 1 deleted: 2 available
  (3200, 'Pull-Down Chrome', 'https://img.example/pulldown.jpg', 'single handle', 129.99, 60.00, 1, 0, 300, 100, 0),
  -- regular stock, no units, price 0
  (3201, 'Bridge Brass',     'https://img.example/bridge.jpg',   'double handle', 0.00,   NULL,  1, 0, 301, 100, 1),
  -- not on display
  (3202, 'Hidden Faucet',    'https://img.example/hidden-f.jpg', 'single handle', 99.00,  40.00, 0, 0, 300, 100, 0),
  -- out of stock
  (3203, 'Out Faucet',       'https://img.example/out-f.jpg',    'double handle', NULL,   NULL,  1, 0, 300, 100, 0),
  -- deleted
  (3204, 'Deleted Faucet',   'https://img.example/del-f.jpg',    'single handle', 50.00,  20.00, 1, 1, 300, 100, 0),
  -- company 101
  (3205, 'Other Co Faucet',  'https://img.example/other-f.jpg',  'single handle', 50.00,  20.00, 1, 0, 302, 101, 0);

INSERT INTO faucets (id, faucet_type_id, is_deleted, slab_id) VALUES
  (6200, 3200, 0, NULL),
  (6201, 3200, 0, NULL),
  (6202, 3200, 0, 6003),
  (6203, 3200, 1, NULL),
  (6204, 3202, 0, NULL),
  (6205, 3204, 0, NULL),
  (6206, 3205, 0, NULL);
