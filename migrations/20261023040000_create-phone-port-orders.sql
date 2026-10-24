-- Granite Phone: moving a company's existing numbers (RingCentral, CloudTalk
-- or another carrier) to Telnyx with port-in orders, run by a company admin
-- from Admin -> Phone. Both tables are new, so this file only adds.
--
-- phone_port_orders: our copy of each Telnyx porting order, so the admin
-- page lists ports without asking Telnyx. Telnyx may split one request into
-- several orders (by carrier, number type or FastPort); those share
-- `request_id`. Refreshed from Telnyx when an order is opened, by Telnyx's
-- porting webhooks and by the phone job. The business details (account
-- number, PIN, address) and the documents stay in Telnyx only.
-- phone_port_events: each order's timeline (status changes, admin actions,
-- the ported numbers added) and the Telnyx notifications already taken
-- (`dedupe_key`).
--
-- All dates are UTC. No triggers, views or generated columns: test-db copies
-- tables only.

CREATE TABLE IF NOT EXISTS phone_port_orders (
  id                   BIGINT            NOT NULL AUTO_INCREMENT,
  company_id           INT               NOT NULL,
  -- the request the order came from: orders Telnyx split from one request share it
  request_id           CHAR(36)          NOT NULL,
  telnyx_order_id      VARCHAR(64)       NOT NULL,
  -- Telnyx's status (draft, in-process, submitted, exception, foc-date-confirmed,
  -- ported, cancel-pending, cancelled), or `deleted` for a draft that is gone
  status               VARCHAR(32)       NOT NULL DEFAULT 'draft',
  -- Telnyx's reasons for the status, [{"code": ..., "description": ...}]
  status_details       JSON              NULL,
  -- the label the numbers get once ported (telnyx_numbers.label)
  label                VARCHAR(100)      NOT NULL DEFAULT '',
  -- the order's numbers, E.164: ["+13175550101", ...]
  phone_numbers        JSON              NOT NULL,
  support_key          VARCHAR(64)       NULL,
  old_carrier          VARCHAR(128)      NULL,
  number_type          VARCHAR(32)       NULL,
  fast_port            TINYINT(1)        NOT NULL DEFAULT 0,
  foc_requested_at     DATETIME          NULL,
  -- the port date the losing carrier confirmed
  foc_at               DATETIME          NULL,
  requirements_met     TINYINT(1)        NOT NULL DEFAULT 0,
  messaging_status     VARCHAR(32)       NULL,
  created_by_user_id   INT               NULL,
  submitted_at         DATETIME          NULL,
  submitted_by_user_id INT               NULL,
  cancel_requested_at  DATETIME          NULL,
  ported_at            DATETIME          NULL,
  -- when the ported numbers were added to telnyx_numbers and set up on Telnyx;
  -- NULL while that is still to do (the phone job retries, backing off)
  completed_at         DATETIME          NULL,
  completion_attempts  SMALLINT UNSIGNED NOT NULL DEFAULT 0,
  completion_tried_at  DATETIME          NULL,
  -- what went wrong last (no personal data), cleared once it works again
  last_error           VARCHAR(500)      NULL,
  last_error_at        DATETIME          NULL,
  -- the last time the order was read from Telnyx
  refreshed_at         DATETIME          NULL,
  telnyx_updated_at    DATETIME(3)       NULL,
  created_at           DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at           DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP
                                         ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_port_orders_telnyx (telnyx_order_id),
  KEY idx_phone_port_orders_company (company_id, id),
  KEY idx_phone_port_orders_refresh (status, refreshed_at),
  CONSTRAINT fk_phone_port_orders_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_port_events (
  id            BIGINT       NOT NULL AUTO_INCREMENT,
  company_id    INT          NOT NULL,
  port_order_id BIGINT       NULL,
  -- created, document, details, submitted, status, foc, comment, split,
  -- cancel_requested, deleted, completed, messaging; `webhook` for a Telnyx
  -- notification taken (not shown on the timeline)
  kind          VARCHAR(32)  NOT NULL,
  -- the order's status after a status change
  status        VARCHAR(32)  NULL,
  -- a short fact without personal data: exception codes, a document kind,
  -- a Telnyx event type
  detail        VARCHAR(255) NULL,
  actor_user_id INT          NULL,
  -- SHA-256 hex of the Telnyx event id of a notification taken; NULL for our own entries
  dedupe_key    CHAR(64)     NULL,
  occurred_at   DATETIME(3)  NOT NULL,
  created_at    DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_port_events_dedupe (dedupe_key),
  KEY idx_phone_port_events_order (port_order_id, occurred_at),
  KEY idx_phone_port_events_company (company_id),
  CONSTRAINT fk_phone_port_events_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
