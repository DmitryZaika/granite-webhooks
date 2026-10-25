-- Onboarding with Dima: the self-serve commitment (Terms section 11).
--
-- company_onboarding_commitments: one row per self-serve company, created at
-- provisioning (companies moved onto a plan by staff get none).
--   status:                pending | booked | completed | waived | suspended |
--                          notice_sent | fee_invoiced | fee_paid | fee_failed |
--                          fee_skipped | void
--                          suspended:   a cancellation request is scheduled
--                                       (withdrawing it resumes the process)
--                          fee_skipped: the deadline passed while
--                                       ONBOARDING_FEE_CHARGING was off (never
--                                       charged later)
--                          void:        the subscription actually ended
--   deadline_at:           the deadline as moved (tolling, extension, a call
--                          Granite cancelled); original_deadline_at: signup + 30 days
--   fee_cents:             2 x (base + add-ons) at signup, before tax, fixed
--   calendly_*, booked_*:  the booked call (booked_at: when we recorded it)
--   availability_checked_on: the last UTC day tolling was decided for
--   notice_cycle_at:       when the current fee notice fell due; the notice
--                          emails are deduped per cycle, so a notice withdrawn
--                          (suspension, moved deadline) and due again is resent
--   charge_after:          notice_sent_at + 7 days
--   version:               bumped on every write (optimistic concurrency)
--
-- company_onboarding_commitment_events: append-only timeline (who, when, what),
-- the evidence if the fee is disputed. actor_user_id is NULL for the system.
--
-- onboarding_availability_days: one row per UTC day: did Dima's Calendly offer
-- a bookable slot within the next 5 business days (result available |
-- no_slots | not_configured | error). Any result but available tolls, and so
-- does a day without a row (the job did not run).
--
-- CREATE TABLE uses IF NOT EXISTS. Safe to run more than once.

CREATE TABLE IF NOT EXISTS company_onboarding_commitments (
  company_id INT NOT NULL PRIMARY KEY,
  status ENUM('pending','booked','completed','waived','suspended','notice_sent',
              'fee_invoiced','fee_paid','fee_failed','fee_skipped','void') NOT NULL,
  deadline_at DATETIME NOT NULL,
  original_deadline_at DATETIME NOT NULL,
  tolled_days INT NOT NULL DEFAULT 0,
  extension_used TINYINT(1) NOT NULL DEFAULT 0,
  fee_cents INT NOT NULL,
  calendly_invitee_uri VARCHAR(255) NULL,
  calendly_event_uri VARCHAR(255) NULL,
  calendly_reschedule_url VARCHAR(255) NULL,
  calendly_cancel_url VARCHAR(255) NULL,
  booked_start_at DATETIME NULL,
  booked_end_at DATETIME NULL,
  booked_at DATETIME NULL,
  completed_at DATETIME NULL,
  completed_by INT NULL,
  no_show_at DATETIME NULL,
  suspended_at DATETIME NULL,
  notice_sent_at DATETIME NULL,
  notice_cycle_at DATETIME NULL,
  charge_after DATETIME NULL,
  stripe_invoice_id VARCHAR(255) NULL,
  availability_checked_on DATE NULL,
  version INT NOT NULL DEFAULT 0,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_onboarding_commitments_status (status, deadline_at),
  KEY idx_onboarding_commitments_invoice (stripe_invoice_id),
  CONSTRAINT fk_onboarding_commitments_company FOREIGN KEY (company_id) REFERENCES company(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS company_onboarding_commitment_events (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  kind VARCHAR(32) NOT NULL,
  from_status VARCHAR(16) NULL,
  to_status VARCHAR(16) NOT NULL,
  deadline_at DATETIME NOT NULL,
  actor_user_id INT NULL,
  detail JSON NULL,
  created_at DATETIME NOT NULL,
  KEY idx_onboarding_commitment_events_company (company_id, id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS onboarding_availability_days (
  checked_on DATE NOT NULL PRIMARY KEY,
  checked_at DATETIME NOT NULL,
  result VARCHAR(16) NOT NULL,
  slots INT NOT NULL DEFAULT 0,
  first_slot_at DATETIME NULL,
  window_start_at DATETIME NOT NULL,
  window_end_at DATETIME NOT NULL,
  event_type_uris JSON NULL,
  error_code VARCHAR(64) NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
