-- Call coaching results (the dashboard's "Ask coach"): one row per scored
-- call. The Remix server kept these in memory and scored in the background;
-- the API scores inside the request and keeps the result here, so a page that
-- stopped waiting still finds it when it polls.
CREATE TABLE IF NOT EXISTS call_coaching_analyses (
  company_id    INT          NOT NULL,
  call_id       VARCHAR(64)  NOT NULL,
  user_id       INT          NOT NULL,
  status        ENUM('pending', 'ready', 'error') NOT NULL,
  score         TINYINT UNSIGNED NULL,
  advice        TEXT         NULL,
  summary       TEXT         NULL,
  customer_name VARCHAR(255) NULL,
  error_message VARCHAR(255) NULL,
  created_at    DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, call_id),
  KEY idx_call_coaching_user (user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
