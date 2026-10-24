-- 10DLC texting registration: US carriers block business texts from local
-- numbers until the company's Brand and Campaign are registered with The
-- Campaign Registry (TCR) through Telnyx. A company's Managers fill the
-- registration in from Admin -> Phone; only a superuser sends it to Telnyx
-- (Telnyx and TCR charge for brands, campaigns and number assignments) and
-- assigns the company's numbers to the approved campaign. Every table is new,
-- so this file only adds.
--
-- tendlc_registrations: one per company. `draft` is the form as last saved
-- ({"brand": {...}, "campaign": {...}}, the API's camelCase fields);
-- `sent_brand` / `sent_campaign` the form's parts as last sent to Telnyx; the
-- Telnyx ids and statuses as last read; Telnyx's reasons (phone numbers
-- masked); who did what when.
-- tendlc_numbers: the company's numbers assigned to the campaign, and
-- Telnyx's answer for each.
-- tendlc_webhook_events: Telnyx's 10DLC notifications already taken.
--
-- All dates are UTC. No triggers, views or generated columns: test-db copies
-- tables only.

CREATE TABLE IF NOT EXISTS tendlc_registrations (
  id                      INT               NOT NULL AUTO_INCREMENT,
  company_id              INT               NOT NULL,
  -- draft, ready, sent, campaign_pending, approved, rejected, suspended
  status                  VARCHAR(24)       NOT NULL DEFAULT 'draft',
  draft                   JSON              NOT NULL,
  sent_brand              JSON              NULL,
  sent_campaign           JSON              NULL,
  telnyx_brand_id         VARCHAR(64)       NULL,
  tcr_brand_id            VARCHAR(32)       NULL,
  -- Telnyx's brand status (OK, REGISTRATION_PENDING, REGISTRATION_FAILED) and
  -- identity status (SELF_DECLARED, VERIFIED, VETTED_VERIFIED, UNVERIFIED)
  brand_status            VARCHAR(32)       NULL,
  brand_identity_status   VARCHAR(32)       NULL,
  -- a brand create whose answer never came: the next send looks the brand up
  -- on Telnyx before creating one
  brand_create_started_at DATETIME          NULL,
  telnyx_campaign_id      VARCHAR(64)       NULL,
  tcr_campaign_id         VARCHAR(32)       NULL,
  -- Telnyx's campaignStatus (TCR_PENDING ... MNO_PROVISIONED)
  campaign_status         VARCHAR(32)       NULL,
  -- the campaign's referenceId is granite-c<company>-r<id>-<campaign_seq>; a
  -- campaign TCR refused is replaced by one with the next number
  campaign_seq            SMALLINT UNSIGNED NOT NULL DEFAULT 1,
  -- Telnyx's reasons, [{"title": ..., "description": ..., "fields": [...]}]
  brand_feedback          JSON              NULL,
  campaign_feedback       JSON              NULL,
  -- what went wrong last (no personal data), cleared once it works again
  last_error              VARCHAR(500)      NULL,
  last_error_at           DATETIME          NULL,
  created_by_user_id      INT               NULL,
  draft_saved_at          DATETIME          NULL,
  draft_saved_by_user_id  INT               NULL,
  ready_at                DATETIME          NULL,
  ready_by_user_id        INT               NULL,
  sent_at                 DATETIME          NULL,
  sent_by_user_id         INT               NULL,
  resent_at               DATETIME          NULL,
  approved_at             DATETIME          NULL,
  rejected_at             DATETIME          NULL,
  -- the last time it was read from Telnyx
  refreshed_at            DATETIME          NULL,
  -- a send in progress: another waits (a lock older than 2 minutes is stale)
  send_lock_at            DATETIME          NULL,
  created_at              DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at              DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP
                                            ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_tendlc_registrations_company (company_id),
  KEY idx_tendlc_registrations_status (status, refreshed_at),
  KEY idx_tendlc_registrations_brand (telnyx_brand_id),
  KEY idx_tendlc_registrations_campaign (telnyx_campaign_id),
  CONSTRAINT fk_tendlc_registrations_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS tendlc_numbers (
  id                  BIGINT       NOT NULL AUTO_INCREMENT,
  company_id          INT          NOT NULL,
  registration_id     INT          NOT NULL,
  e164                VARCHAR(20)  NOT NULL,
  -- the Telnyx campaign it was assigned to
  campaign_id         VARCHAR(64)  NOT NULL,
  -- Telnyx's assignmentStatus (PENDING_ASSIGNMENT, ASSIGNED, FAILED_ASSIGNMENT,
  -- PENDING_UNASSIGNMENT, FAILED_UNASSIGNMENT), or DELETED
  status              VARCHAR(32)  NOT NULL,
  -- Telnyx's reason when it failed (phone numbers masked)
  error               VARCHAR(300) NULL,
  assigned_by_user_id INT          NULL,
  -- the last time it was read from Telnyx
  refreshed_at        DATETIME     NULL,
  created_at          DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                   ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_tendlc_numbers_number (company_id, e164),
  KEY idx_tendlc_numbers_registration (registration_id),
  KEY idx_tendlc_numbers_status (status, refreshed_at),
  CONSTRAINT fk_tendlc_numbers_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS tendlc_webhook_events (
  id          BIGINT      NOT NULL AUTO_INCREMENT,
  company_id  INT         NOT NULL,
  -- 10dlc.brand.update, 10dlc.campaign.update or 10dlc.phone_number.update
  event_type  VARCHAR(64) NOT NULL,
  -- SHA-256 hex of Telnyx's event id
  dedupe_key  CHAR(64)    NOT NULL,
  received_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_tendlc_webhook_events_dedupe (dedupe_key),
  KEY idx_tendlc_webhook_events_company (company_id),
  CONSTRAINT fk_tendlc_webhook_events_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
