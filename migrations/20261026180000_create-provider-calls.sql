-- A company's CloudTalk and RingCentral calls, so its call history stays on
-- deals and customers after it changes provider (the providers' APIs only
-- answer while the company is connected).
--
-- provider_calls: one row per call, pulled by POST /v1/jobs/call-history on
-- every time-triggered tick. item is the call as GET /v1/calls/{provider}
-- returns it (the CloudTalk shape). customer_digits: the last 10 digits of
-- the outside number. started_at in UTC.
--
-- provider_call_syncs: how far the job has read each company's call log.
-- The first runs walk it a month at a time from the company's first
-- customer; after that each run reads from the last one (less a day).

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS provider_calls (
  id              BIGINT       NOT NULL AUTO_INCREMENT,
  company_id      INT          NOT NULL,
  provider        ENUM('cloudtalk', 'ringcentral') NOT NULL,
  call_id         VARCHAR(64)  NOT NULL,
  customer_digits CHAR(10)     NOT NULL,
  started_at      DATETIME     NOT NULL,
  item            JSON         NOT NULL,
  created_at      DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at      DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                               ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_provider_calls_call (company_id, provider, call_id),
  KEY idx_provider_calls_customer (company_id, customer_digits, started_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS provider_call_syncs (
  company_id INT      NOT NULL,
  provider   ENUM('cloudtalk', 'ringcentral') NOT NULL,
  synced_to  DATETIME NOT NULL,
  PRIMARY KEY (company_id, provider)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
