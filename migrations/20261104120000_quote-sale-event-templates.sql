-- Admin > Quotes > Automation > Event templates: selling a quote adds one
-- unscheduled event per template to the template's calendar, titled with a
-- variable (the quote's or the customer's name) and the text after it, e.g.
-- "Ostrom Kitchen - Plumbing".
CREATE TABLE IF NOT EXISTS quote_sale_event_templates (
  id          INT          NOT NULL AUTO_INCREMENT,
  company_id  INT          NOT NULL,
  calendar_id INT          NOT NULL,
  -- What the title starts with: the quote's name or the customer's name
  title_variable ENUM('quote_name', 'customer_name') NOT NULL DEFAULT 'quote_name',
  -- What follows the variable ("Plumbing"); empty: the variable alone
  title_text  VARCHAR(200) NOT NULL DEFAULT '',
  created_by  INT          NULL,
  created_at  DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at  DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  KEY idx_quote_sale_event_templates_company (company_id),
  KEY idx_quote_sale_event_templates_calendar (calendar_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
