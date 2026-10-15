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

-- ---- Stone slabs dialog (GET /v1/stones/{id}/slabs, PATCH /v1/slabs/{id})
-- Stone 3010 is hidden from the catalogs (is_display = 0) so the list pages
-- above keep their rows; the dialog opens by URL. Bundles sort naturally
-- (Slab 1, Slab 2, Slab 10) and remnants follow their parent slab.
INSERT INTO stones (id, name, type, url, company_id, supplier_id, is_display, on_sale, regular_stock, length, width, retail_price, cost_per_sqft, level, finishing, samples_amount, bundle_number, bundle_location, created_date) VALUES
  (3010, 'Taj Mahal', 'quartzite', 'https://img.example/taj-mahal.jpg', 100, 300, 0, 0, 0, 125, 64, 120, 80, 6, 'polished', 0, 'TM-7', 'Rack 9', '2026-03-01 09:30:00');

-- Sale 7010: Alice, sold by Rita, every detail the tooltip shows.
-- Sale 7011: Zoë, sold by Mark, from a remnant of 7010's slab; no details.
-- Sale 7012: no customer row and no seller: shown as sold with no details.
INSERT INTO sales (id, customer_id, seller_id, company_id, price, status, project_address, sale_date, notes, square_feet) VALUES
  (7010, 1000, 100, 100, 4200.00, 'pending', '1 Main St',  '2026-04-15 15:00:00', 'Deliver after 5pm', 55.50),
  (7011, 1010, 101, 100, 900.00,  'pending', 'Straße 7',   '2026-04-20 10:00:00', NULL,                0.00),
  (7012, NULL, NULL, 100, 100.00, 'pending', 'Nowhere',    '2026-04-21 10:00:00', NULL,                NULL);

INSERT INTO slab_inventory (id, stone_id, bundle, url, length, width, sale_id, parent_id, cut_date, deleted_at, notes, room, square_feet) VALUES
  (6070, 3010, 'Slab 10', 'https://img.example/tm-10.jpg', '126', '63', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
  (6071, 3010, 'Slab 2',  'https://img.example/tm-2.jpg',  '125', '64', 7010, NULL, NULL, NULL, 'Edge polished', 'Kitchen', 30.25),
  (6072, 3010, 'Slab 2',  'https://img.example/tm-2.jpg',  '60',  '30', NULL, 6071, NULL, NULL, NULL, NULL, NULL),
  (6073, 3010, 'Slab 2',  'https://img.example/tm-2.jpg',  '40',  '20', 7011, 6071, NULL, NULL, NULL, 'Bath', 0),
  (6074, 3010, 'Slab 1',  NULL,                            '124', '62', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
  (6075, 3010, 'Slab 3',  'https://img.example/tm-3.jpg',  '125', '64', NULL, NULL, '2026-04-01 10:00:00', NULL, NULL, NULL, NULL),
  (6076, 3010, 'Slab 4',  'https://img.example/tm-4.jpg',  '125', '64', NULL, NULL, NULL, '2026-04-02 10:00:00', NULL, NULL, NULL),
  (6077, 3010, 'Slab 5',  'https://img.example/tm-5.jpg',  '125', '64', 7012, NULL, NULL, NULL, NULL, NULL, NULL);

-- Sinks installed on sale 7010's slab: 2 x Composite Black, 1 x Bar Prep Sink
-- (Composite Black is not on display, Bar Prep keeps its one free unit).
INSERT INTO sinks (id, sink_type_id, is_deleted, slab_id) VALUES
  (6109, 3102, 0, 6071),
  (6110, 3102, 0, 6071),
  (6111, 3107, 0, 6071);

-- ---- Stone types (quick-add stone, stone forms). Ids 100+.
INSERT INTO stone_types (id, name, company_id) VALUES
  (100, 'granite',   100),
  (101, 'quartz',    100),
  (102, 'marble',    100),
  (103, 'quartzite', 100),
  (104, 'granite',   101);

-- ---- Sink types (sink forms). Ids 100+.
INSERT INTO sink_types (id, name, company_id) VALUES
  (100, 'stainless 18 gauge', 100),
  (101, 'stainless 16 gauge', 100),
  (102, 'composite',          100),
  (103, 'ceramic',            100),
  (104, 'farm house',         100),
  (105, 'composite',          101);

-- ---- Installed (project) photos of stones (admin stone images tab). Ids 100+.
INSERT INTO installed_stones (id, url, stone_id) VALUES
  (100, 'https://img.example/installed-black-1.jpg', 3000),
  (101, 'https://img.example/installed-black-2.jpg', 3000),
  (102, 'https://img.example/installed-zebra-1.jpg', 3008),
  (103, 'https://img.example/installed-other-1.jpg', 4000);

-- ---- Installed photos of sink and faucet models. Ids 100+.
INSERT INTO installed_sinks (id, url, sink_id) VALUES
  (100, 'https://img.example/installed-sink-1.jpg', 3100),
  (101, 'https://img.example/installed-sink-other.jpg', 3106);
INSERT INTO installed_faucets (id, url, faucet_id) VALUES
  (100, 'https://img.example/installed-faucet-1.jpg', 3200),
  (101, 'https://img.example/installed-faucet-other.jpg', 3205);

-- ---- Public showroom: company 100 answers on the "acme" subdomain
-- (/customer/stones redirects to /customer/100/stones).
UPDATE company SET subdomain = 'acme' WHERE id = 100;

-- ---- Employee dashboard (/employee): sales stats, conversion, leaderboard,
-- sales goal, widget layout, Telegram. Dates are relative to the current month
-- so "this month" widgets always have rows: @m0 is the 1st at 00:00 (every
-- "this month" row is on the 1st, so it is never in the future), @prev a day
-- of last month. Ids: groups/lists 100+, deals 8000+, stage history 8100+,
-- calls 8200+, SMS 8300+, emails 8400+, points ledger 8500+.
SET @m0 = CAST(DATE_FORMAT(CURRENT_DATE, '%Y-%m-01') AS DATETIME);
SET @prev = @m0 - INTERVAL 1 MONTH + INTERVAL 4 DAY;

-- Every points source counts from 2026-01-01 (the migrations date some at the
-- moment they ran, which would hide the seeded activity).
UPDATE activity_points_sources SET counts_from = '2026-01-01 00:00:00';

-- Company 100 shows the leaderboard; its Onboarding switch stays off (tour
-- points are not counted). Company 101 has inbound email and CloudTalk (fake
-- credentials), so its dashboard offers the Sent emails and Sent SMS widgets.
UPDATE company SET leaderboard_enabled = 1 WHERE id = 100;
UPDATE company SET domain = 'other.test', cloudtalk_access_key = 'seed-key',
                   cloudtalk_access_secret = 'seed-secret' WHERE id = 101;

-- Rita (100) has no goal (default $50,000) and no Telegram; Mark (101) has
-- both bots linked and a $75,000 goal.
UPDATE users SET monthly_sales_goal = 75000, telegram_id = 900101,
                 notifications_telegram_id = 910101 WHERE id = 101;

INSERT INTO groups_list (id, name, company_id, is_default) VALUES
  (100, 'Sales', 100, 1),
  (101, 'Other Sales', 101, 1);

INSERT INTO deals_list (id, name, position, group_id, deleted_at) VALUES
  (100, 'New',      0, 100, NULL),
  (101, 'Quoted',   1, 100, NULL),
  (102, 'Archived', 2, 100, '2026-01-01 00:00:00'),
  (103, 'New',      0, 101, NULL);

-- Rita's deals. Counted this month: won 8000 + 8010 ($2,200), lost 8001,
-- created 8001 + 8002; open: 8002 + 8003 ($2,500.50). All time (conversion):
-- 8000, 8001, 8002, 8003, 8004, 8010. Never counted: 8005 (deleted), 8006
-- (deleted customer), 8007 (deleted list). 8008 is Mark's, 8009 company 101's.
INSERT INTO deals (id, customer_id, amount, title, list_id, position, user_id, is_won, created_at, updated_at, deleted_at) VALUES
  (8000, 1000, 1000.00, 'Alice kitchen',   101, 0, 100, 1,    @prev,                  @m0 + INTERVAL 9 HOUR,  NULL),
  (8001, 1001,  500.00, 'Bob vanity',      100, 0, 100, 0,    @m0 + INTERVAL 1 HOUR,  @m0 + INTERVAL 8 HOUR,  NULL),
  (8002, 1004, 2500.50, 'Erin countertops',101, 1, 100, NULL, @m0 + INTERVAL 2 HOUR,  @m0 + INTERVAL 3 HOUR,  NULL),
  (8003, 1008,    NULL, 'Ivy island',      100, 1, 100, NULL, @prev,                  @prev,                  NULL),
  (8004, 1000, 9999.00, 'Alice bathroom',  101, 2, 100, 1,    @prev,                  @prev,                  NULL),
  (8005, 1000, 4000.00, 'Deleted deal',    101, 3, 100, 1,    @m0 + INTERVAL 1 HOUR,  @m0 + INTERVAL 1 HOUR,  @m0 + INTERVAL 2 HOUR),
  (8006, 1003, 4000.00, 'Deleted customer',101, 4, 100, 1,    @m0 + INTERVAL 1 HOUR,  @m0 + INTERVAL 1 HOUR,  NULL),
  (8007, 1000,  777.00, 'Archived list',   102, 0, 100, NULL, @m0 + INTERVAL 1 HOUR,  @m0 + INTERVAL 1 HOUR,  NULL),
  (8008, 1010, 3000.00, 'Zoe kitchen',     101, 5, 101, 1,    @prev,                  @m0 + INTERVAL 5 HOUR,  NULL),
  (8009, 2000, 8000.00, 'Other company',   103, 0, 106, 1,    @prev,                  @m0 + INTERVAL 5 HOUR,  NULL),
  (8010, 1004, 1200.00, 'Erin backsplash', 101, 6, 100, 1,    @prev,                  @m0 + INTERVAL 10 HOUR, NULL);

-- Stage moves: a deal's first row is its creation and never scores. 8002
-- advanced this month, 8000 last month (3 points each).
INSERT INTO deal_stage_history (id, deal_id, list_id, entered_at) VALUES
  (8100, 8002, 100, @m0 + INTERVAL 2 HOUR),
  (8101, 8002, 101, @m0 + INTERVAL 3 HOUR),
  (8102, 8000, 100, @prev),
  (8103, 8000, 101, @prev + INTERVAL 1 HOUR);

-- Rita's calls: a conversation (10) and two attempts to one number on one
-- day (only the first scores, 2) this month; a voicemail (5) last month; a
-- pending call scores nothing.
INSERT INTO phone_call_points (id, company_id, provider, provider_call_id, user_id, customer_digits, started_at, talk_seconds, outcome) VALUES
  (8200, 100, 'cloudtalk', 'seed-8200', 100, '3175550100', @m0 + INTERVAL 1 HOUR, 300, 'conversation'),
  (8201, 100, 'cloudtalk', 'seed-8201', 100, '3175550102', @m0 + INTERVAL 2 HOUR,  10, 'attempt'),
  (8202, 100, 'cloudtalk', 'seed-8202', 100, '3175550102', @m0 + INTERVAL 3 HOUR,  12, 'attempt'),
  (8203, 100, 'cloudtalk', 'seed-8203', 100, '3175550105', @prev,                  45, 'voicemail'),
  (8204, 100, 'cloudtalk', 'seed-8204', 100, '3175550100', @m0 + INTERVAL 4 HOUR,  60, 'pending');

-- Rita's hand-sent texts: four to one number on one day (three score) and an
-- automated one (never scores). Olga's two texts feed company 101's Sent SMS.
INSERT INTO cloudtalk_sms (id, recipient, text, direction, status, sender_user_id, company_id, recipient10, created_date, is_automated) VALUES
  (8300, 3175550100, 'Hi Alice',  'outbound', 'sent', 100, 100, '3175550100', @m0 + INTERVAL 1 HOUR, 0),
  (8301, 3175550100, 'Quote',     'outbound', 'sent', 100, 100, '3175550100', @m0 + INTERVAL 2 HOUR, 0),
  (8302, 3175550100, 'Reminder',  'outbound', 'sent', 100, 100, '3175550100', @m0 + INTERVAL 3 HOUR, 0),
  (8303, 3175550100, 'Fourth',    'outbound', 'sent', 100, 100, '3175550100', @m0 + INTERVAL 4 HOUR, 0),
  (8304, 3175550100, 'Flow text', 'outbound', 'sent', 100, 100, '3175550100', @m0 + INTERVAL 5 HOUR, 1),
  (8305, 4635550200, 'Hello',     'outbound', 'sent', 106, 101, '4635550200', @m0 + INTERVAL 1 HOUR, 0),
  (8306, 4635550200, 'Again',     'outbound', 'sent', 106, 101, '4635550200', @prev,                 0);

-- Emails: Rita's two sent this month and one last month score; a draft and
-- one never sent (no message id) do not. Olga's feed company 101's Sent emails
-- (3 this month, 1 last month).
INSERT INTO emails (id, subject, body, sent_at, message_id, sender_user_id, company_id, is_draft) VALUES
  (8400, 'Quote',     'b', @m0 + INTERVAL 1 HOUR, '<seed-8400@acme.test>', 100, 100, 0),
  (8401, 'Follow up', 'b', @m0 + INTERVAL 2 HOUR, '<seed-8401@acme.test>', 100, 100, 0),
  (8402, 'Old',       'b', @prev,                 '<seed-8402@acme.test>', 100, 100, 0),
  (8403, 'Draft',     'b', @m0 + INTERVAL 3 HOUR, NULL,                    100, 100, 1),
  (8404, 'Unsent',    'b', @m0 + INTERVAL 4 HOUR, NULL,                    100, 100, 0),
  (8405, 'Hi',        'b', @m0 + INTERVAL 1 HOUR, '<seed-8405@other.test>', 106, 101, 0),
  (8406, 'Hi again',  'b', @m0 + INTERVAL 2 HOUR, '<seed-8406@other.test>', 106, 101, 0),
  (8407, 'Third',     'b', @m0 + INTERVAL 3 HOUR, '<seed-8407@other.test>', 106, 101, 0),
  (8408, 'Older',     'b', @prev,                 '<seed-8408@other.test>', 106, 101, 0);

-- Tour points: only counted while the company's Onboarding switch is on.
INSERT INTO user_points_ledger (id, user_id, company_id, task_id, points, reason, awarded_at) VALUES
  (8500, 100, 100, 'dashboard-overview', 40, 'completed', @m0 + INTERVAL 1 HOUR);

-- ---- Post-installation checklists (GET /v1/checklists). Ids 9000+.
-- Newest first, ties by id: 9003 and 9001 share a timestamp, 02:00 UTC (the
-- previous day in Indianapolis). 9001 has no installer.
-- 9002's installer is a deleted user (still named), its comment is empty.
-- 9004 is a real row named like the onboarding sample (the page hides it).
-- 9005 has no company and 9006 is company 101's: never listed for 100.
INSERT INTO checklists (id, customer_id, installer_id, company_id, customer_name, installation_address, email, material_correct, seams_satisfaction, appliances_fit, backsplashes_correct, edges_correct, holes_drilled, cleanup_completed, comments, signature, created_at) VALUES
  (9000, 1000, 100, 100, 'Alice Lead',     '1 Main St, Indianapolis, IN', 'alice@example.test', 1, 1, 1, 1, 1, 1, 1, 'Great job, very clean.', 'data:image/png;base64,c2ln', '2026-05-10 14:00:00'),
  (9001, NULL, NULL, 100, 'Bob Walk-in',    '22 Side Rd, Carmel, IN',      NULL,                 1, 0, 1, 0, 1, 0, 0, NULL,                     'data:image/png;base64,c2ln', '2026-05-12 02:00:00'),
  (9002, 1004, 105, 100, 'Erin Builder',   '5 Oak Ave',                   NULL,                 0, 1, 0, 1, 0, 1, 1, '',                       'data:image/png;base64,c2ln', '2026-05-11 14:00:00'),
  (9003, 1010, 101, 100, 'Zoë Ünicode-Łukasz', 'Straße 7',                'zoe@example.test',   0, 0, 0, 0, 0, 0, 0, 'Seam visible near sink; customer wants a callback.', 'data:image/png;base64,c2ln', '2026-05-12 02:00:00'),
  (9004, NULL, 100, 100, 'Onboarding Sample Checklist Customer', '456 Install Avenue, Indianapolis, IN 46202', NULL, 1, 1, 1, 1, 1, 1, 1, NULL, 'data:image/png;base64,c2ln', '2026-05-09 14:00:00'),
  (9005, NULL, 100, NULL, 'No Company',    '9 Nowhere',                   NULL,                 1, 1, 1, 1, 1, 1, 1, NULL,                     'data:image/png;base64,c2ln', '2026-05-13 14:00:00'),
  (9006, 2000, 106, 101, 'Other Co Customer', '1 Other St',               NULL,                 1, 1, 1, 1, 1, 1, 1, 'Other company',        'data:image/png;base64,c2ln', '2026-05-13 14:00:00');

-- ---- Deals board (GET /v1/deals, /v1/deal-groups, activities, notes). Ids 8600+.
-- Dina (107) owns these deals so Rita's sales stats stay as they are. She is
-- an employee without positions (not a sales rep, not on the leaderboard).
-- Groups: 100 'Sales' (default), 104 'Remodel', 105 hidden. Stages 100/101 in
-- 100 (102 deleted), 104/105 in 104, 106 in hidden 105.
-- Open deals in group 100: 8600 (stage 100, pos 0), 8601 (100, pos 1), 8602
-- (101); won 8603, lost 8604 (stage 101); 8605 deleted; 8606 deleted customer
-- 1003; 8607 in group 104; 8608 is Rita's (not Dina's board).
-- 8600 has an overdue + a future activity, a note with two comments and a
-- deleted comment, photos, an email; 8601 has one undated activity, a deleted
-- and a completed one; 8602 has a deleted note.
INSERT INTO users (id, email, password, name, is_employee, is_admin, is_superuser, is_deleted, company_id) VALUES
  (107, 'dealer@acme.test', '$2b$10$daaV4OW7Z3vjsAwLf.2lTeFKeDjT0vAMrimETNcoQ3i//afF85GEa', 'Dina Dealer', 1, 0, 0, 0, 100);

INSERT INTO sessions (id, user_id, expiration_date, is_deleted, created_date) VALUES
  ('aaaaaaaa-0000-4000-8000-000000000107', 107, '2037-12-31 00:00:00', 0, CURRENT_TIMESTAMP);

-- Nina (8695) is a lead of Rita's that only Dina's deals use.
INSERT INTO customers (id, name, phone, company_id, source, sales_rep, created_date, customer_temperature) VALUES
  (8695, 'Nina Lead', '317-555-0195', 100, 'leads', 100, '2026-03-30 08:00:00', 'hot');
INSERT INTO customers_emails (id, customer_id, email, created_at, created_by) VALUES
  (8696, 8695, 'nina@example.com', '2026-03-30 08:00:00', 100),
  (8697, 8695, 'nina.work@example.com', '2026-03-30 09:00:00', 100);

INSERT INTO groups_list (id, name, company_id, is_default, is_displayed) VALUES
  (104, 'Remodel', 100, 0, 1),
  (105, 'Hidden',  100, 0, 0);

INSERT INTO deals_list (id, name, position, group_id, deleted_at) VALUES
  (104, 'Measure', 0, 104, NULL),
  (105, 'Install', 1, 104, NULL),
  (106, 'Parked',  0, 105, NULL);

INSERT INTO deals (id, customer_id, amount, title, status, list_id, position, user_id, is_won, lost_reason, due_date, created_at, updated_at, deleted_at, created_by) VALUES
  (8600, 8695, 1500.25, 'Dina kitchen',   'New',     100, 0, 107, NULL, NULL,        '2026-09-30', '2026-04-01 10:00:00', '2026-04-01 10:00:00', NULL, 'Dina Dealer'),
  (8601, 1004, NULL,    NULL,             'New',     100, 1, 107, NULL, NULL,        NULL,         '2026-04-02 10:00:00', '2026-04-02 10:00:00', NULL, NULL),
  (8602, 1010, 300.00,  'Dina vanity',    'Quoted',  101, 0, 107, NULL, NULL,        NULL,         '2026-04-03 10:00:00', '2026-04-03 10:00:00', NULL, NULL),
  (8603, 1008, 999.00,  'Dina won',       'Quoted',  101, 1, 107, 1,    NULL,        NULL,         '2026-04-04 10:00:00', '2026-04-04 10:00:00', NULL, NULL),
  (8604, 1011, 50.00,   'Dina lost',      'Quoted',  101, 2, 107, 0,    'Too pricey',NULL,         '2026-04-05 10:00:00', '2026-04-05 10:00:00', NULL, NULL),
  (8605, 8695, 10.00,   'Dina deleted',   'New',     100, 2, 107, NULL, NULL,        NULL,         '2026-04-06 10:00:00', '2026-04-06 10:00:00', '2026-04-07 00:00:00', NULL),
  (8606, 1003, 20.00,   'Dina gone cust', 'New',     100, 3, 107, NULL, NULL,        NULL,         '2026-04-07 10:00:00', '2026-04-07 10:00:00', NULL, NULL),
  (8607, 8695, 30.00,   'Dina remodel',   'Measure', 104, 0, 107, NULL, NULL,        NULL,         '2026-04-08 10:00:00', '2026-04-08 10:00:00', NULL, NULL);

INSERT INTO deal_stage_history (id, deal_id, list_id, entered_at, exited_at) VALUES
  (8610, 8602, 100, '2026-04-03 10:00:00', '2026-04-03 12:00:00'),
  (8611, 8602, 101, '2026-04-03 12:00:00', NULL),
  (8612, 8603, 101, '2026-04-04 10:00:00', '2026-04-10 09:00:00');

INSERT INTO deal_activities (id, deal_id, company_id, name, deadline, priority, is_completed, completed_at, created_at, deleted_at, created_by) VALUES
  (8620, 8600, 100, 'Call back',     '2026-01-05 09:30:00', 'high',   0, NULL,                  '2026-04-01 11:00:00', NULL,                  'Dina Dealer'),
  (8621, 8600, 100, 'Send quote',    '2099-01-01 00:00:00', 'low',    0, NULL,                  '2026-04-01 12:00:00', NULL,                  'Dina Dealer'),
  (8622, 8601, 100, 'Measure',       NULL,                  'medium', 0, NULL,                  '2026-04-02 11:00:00', NULL,                  'Dina Dealer'),
  (8623, 8601, 100, 'Old task',      '2026-01-01 10:00:00', 'medium', 1, '2026-01-02 10:00:00', '2026-04-02 12:00:00', NULL,                  'Dina Dealer'),
  (8624, 8601, 100, 'Deleted task',  '2026-01-01 10:00:00', 'medium', 0, NULL,                  '2026-04-02 13:00:00', '2026-04-03 00:00:00', 'Dina Dealer'),
  (8625, 8603, 100, 'Call back',     '2026-01-06 09:00:00', 'medium', 0, NULL,                  '2026-04-04 11:00:00', NULL,                  'Dina Dealer');

INSERT INTO deal_notes (id, deal_id, company_id, content, is_pinned, created_at, created_by, deleted_at) VALUES
  (8630, 8600, 100, 'Prefers white quartz', 1, '2026-04-01 13:00:00', 'Dina Dealer', NULL),
  (8631, 8600, 100, 'Second note',          0, '2026-04-01 14:00:00', 'Dina Dealer', NULL),
  (8632, 8602, 100, 'Deleted note',         0, '2026-04-03 13:00:00', 'Dina Dealer', '2026-04-04 00:00:00');

INSERT INTO deal_note_comments (id, note_id, company_id, content, created_at, created_by, deleted_at) VALUES
  (8640, 8630, 100, 'Agreed',          '2026-04-01 15:00:00', 'Ada Admin', NULL),
  (8641, 8630, 100, 'Deleted comment', '2026-04-01 16:00:00', NULL,        '2026-04-02 00:00:00'),
  (8642, 8630, 100, 'Anonymous',       '2026-04-01 17:00:00', NULL,        NULL);

INSERT INTO deals_images (id, deal_id, image_url, created_at) VALUES
  (8650, 8600, 'https://example.test/deal-8600.jpg', '2026-04-01 10:00:00');

INSERT INTO emails (id, subject, body, sent_at, message_id, sender_user_id, company_id, is_draft, deal_id) VALUES
  (8660, 'Your quote', 'b', '2026-04-01 10:00:00', '<seed-8660@acme.test>', 107, 100, 0, 8600);

-- Drip templates of stage 101 for company 100 (lead customers only).
INSERT INTO email_templates (id, template_name, template_subject, template_body, company_id, user_id, lead_list_id, hour_delay, show_template) VALUES
  (8670, 'Quoted drip', 'Your quote', 'Hi', 100, NULL, 101, 2, 1);
INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, lead_list_id, hour_delay, position) VALUES
  (8671, 100, NULL, 'Quoted sms', 'Hi', 101, 1, 0);

-- ---- Sales questionnaires (/v1/questionnaires, /v1/deals/{id}/questionnaires). Ids 8720+.
-- 8720 shared by company 100, 8721 Dina's own, 8722 Rita's own (not Dina's),
-- 8723 company 101's, 8724 deleted. Question 8732 is deleted. Deal 8600 has
-- an answer to 8730.
INSERT INTO sales_questionnaires (id, name, company_id, user_id, created_at, deleted_at) VALUES
  (8720, 'Remodel Discovery', 100, NULL, '2026-04-01 10:00:00', NULL),
  (8721, 'Dina checklist',    100, 107,  '2026-04-02 10:00:00', NULL),
  (8722, 'Rita checklist',    100, 100,  '2026-04-03 10:00:00', NULL),
  (8723, 'Other company',     101, NULL, '2026-04-04 10:00:00', NULL),
  (8724, 'Deleted',           100, 107,  '2026-04-05 10:00:00', '2026-04-06 00:00:00');
INSERT INTO sales_questionnaire_questions (id, questionnaire_id, question, deleted_at) VALUES
  (8730, 8720, 'What type of remodel?', NULL),
  (8731, 8720, 'Budget?',               NULL),
  (8732, 8720, 'Old question',          '2026-04-02 00:00:00'),
  (8733, 8721, 'Sink style?',           NULL),
  (8734, 8722, 'Rita only?',            NULL),
  (8735, 8723, 'Other?',                NULL);
INSERT INTO sales_questionnaire_responses (deal_id, question_id, answer, answered_by) VALUES
  (8600, 8730, 'Kitchen', 107);

-- ---- Mailbox (/v1/emails). Ids 8800+, all in April 2026 (never "this month").
-- Olga (106, rep@other.test) of company 101, which has a mail domain.
-- T1 ...0001: customer buyer@other-customer.test (customer 2000) wrote twice
--   (8800 read, 8802 unread, with an attachment), Olga replied (8801, opened
--   by the customer). On deal 8009.
-- T2 ...0002: Pat Prospect (no customer) wrote, read; the name is only in the
--   participant row.
-- T3 ...0003: Olga wrote to the customer only (Sent), opened 1 s after the
--   send (an image proxy: not a real open).
-- T4 ...0004: trashed inbound mail.
-- T5 ...0005: Olga's draft.
-- T6 ...0006: company 100 mail to rep@acme.test (never Olga's).
INSERT INTO customers_emails (id, customer_id, email, created_at, created_by) VALUES
  (8790, 2000, 'buyer@other-customer.test', '2026-04-01 08:00:00', 106);

INSERT INTO emails (id, subject, body, html_body, sent_at, deleted_at, message_id, thread_id, sender_user_id, receiver_user_id, sender_email, receiver_email, deal_id, employee_read_at, company_id, is_draft) VALUES
  (8800, 'Kitchen quote',     'Can you quote my kitchen?', NULL, '2026-04-10 10:00:00', NULL, '<m8800@other-customer.test>', 'aaaaaaaa-1111-4000-8000-000000000001', NULL, 106, 'buyer@other-customer.test', 'rep@other.test', 8009, '2026-04-10 10:30:00', 101, 0),
  (8801, 'Re: Kitchen quote', 'Sure, attached.',           '<p>Sure, attached.</p>', '2026-04-10 11:00:00', NULL, '<m8801@other.test>', 'aaaaaaaa-1111-4000-8000-000000000001', 106, NULL, 'Olga Other <rep@other.test>', 'buyer@other-customer.test', 8009, NULL, 101, 0),
  (8802, 'Re: Kitchen quote', 'Thanks! See my sketch.',    NULL, '2026-04-11 09:00:00', NULL, '<m8802@other-customer.test>', 'aaaaaaaa-1111-4000-8000-000000000001', NULL, 106, 'buyer@other-customer.test', 'rep@other.test', NULL, NULL, 101, 0),
  (8803, 'Granite question',  'Do you carry Black Pearl?', NULL, '2026-04-05 12:00:00', NULL, '<m8803@prospect.test>', 'aaaaaaaa-1111-4000-8000-000000000002', NULL, NULL, 'pat@prospect.test', 'rep@other.test', NULL, '2026-04-05 13:00:00', 101, 0),
  (8804, 'Following up',      'Just checking in.',         NULL, '2026-04-06 12:00:00', NULL, '<m8804@other.test>', 'aaaaaaaa-1111-4000-8000-000000000003', 106, NULL, 'rep@other.test', 'buyer@other-customer.test', NULL, NULL, 101, 0),
  (8805, 'Spam',              'Buy now',                   NULL, '2026-04-07 12:00:00', '2026-04-08 12:00:00', '<m8805@spam.test>', 'aaaaaaaa-1111-4000-8000-000000000004', NULL, NULL, 'spam@spam.test', 'rep@other.test', NULL, NULL, 101, 0),
  (8806, 'Draft quote',       'Work in progress',          NULL, '2026-04-09 12:00:00', NULL, NULL, 'aaaaaaaa-1111-4000-8000-000000000005', 106, NULL, 'rep@other.test', 'buyer@other-customer.test', NULL, NULL, 101, 1),
  (8807, 'Acme only',         'Not for Olga',              NULL, '2026-04-08 12:00:00', NULL, '<m8807@x.test>', 'aaaaaaaa-1111-4000-8000-000000000006', NULL, 100, 'someone@x.test', 'rep@acme.test', NULL, NULL, 100, 0);

INSERT INTO email_participants (email_id, email, display_name, user_id, customer_id, type, position) VALUES
  (8800, 'buyer@other-customer.test', 'Bea Buyer', NULL, 2000, 'from', 0),
  (8800, 'rep@other.test', NULL, 106, NULL, 'to', 0),
  (8801, 'rep@other.test', 'Olga Other', 106, NULL, 'from', 0),
  (8801, 'buyer@other-customer.test', NULL, NULL, 2000, 'to', 0),
  (8801, 'boss@other-customer.test', 'Boss', NULL, NULL, 'cc', 0),
  (8801, 'secret@other.test', NULL, NULL, NULL, 'bcc', 0),
  (8803, 'pat@prospect.test', 'Pat Prospect', NULL, NULL, 'from', 0),
  (8803, 'rep@other.test', NULL, 106, NULL, 'to', 0);

INSERT INTO email_attachments (id, email_id, content_type, content_subtype, filename, url) VALUES
  (8810, 8802, 'image', 'png', 'sketch.png', 'https://example.test/sketch.png');

INSERT INTO email_reads (message_id, read_at) VALUES
  ('<m8801@other.test>', '2026-04-10 15:00:00'),
  ('<m8804@other.test>', '2026-04-06 12:00:01');
