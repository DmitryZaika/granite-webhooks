-- Base pay an admin sets per rep (Admin > Quotes > Commissions > Base pay):
-- an amount paid every week, every two weeks or every month from the first
-- payday (monthly: on that day of the month, or the month's last day when
-- shorter), until an optional last day. It replaces the company-wide weekly
-- base (commission_settings.base_weekly, no longer read).
CREATE TABLE commission_base_pay (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NOT NULL,
  amount DECIMAL(10,2) NOT NULL,
  frequency VARCHAR(10) NOT NULL,
  start_date DATE NOT NULL,
  end_date DATE NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_commission_base_pay_company (company_id, deleted_at),
  CONSTRAINT fk_commission_base_pay_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
