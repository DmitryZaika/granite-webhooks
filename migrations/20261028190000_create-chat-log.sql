-- Every question asked in the sidebar Chat and the answer it got, kept for
-- good: chat_history holds only each user's current conversation and a new
-- chat overwrites it. from_manual: the answer came from the CRM manual.
CREATE TABLE chat_log (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  user_id INT NOT NULL,
  company_id INT NULL,
  question TEXT NOT NULL,
  answer MEDIUMTEXT NOT NULL,
  from_manual TINYINT(1) NOT NULL DEFAULT 0,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_chat_log_company_created (company_id, created_at),
  KEY idx_chat_log_user_created (user_id, created_at)
);
