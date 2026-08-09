ALTER TABLE email_templates
  DROP INDEX uk_email_templates_group;

ALTER TABLE email_templates
  ADD COLUMN lead_list_id INT NULL AFTER lead_group_id,
  ADD CONSTRAINT fk_email_templates_lead_list_id
    FOREIGN KEY (lead_list_id) REFERENCES deals_list(id);

UPDATE email_templates et
JOIN (
  SELECT dl.group_id, MIN(dl.id) AS list_id
  FROM deals_list dl
  INNER JOIN (
    SELECT group_id, MIN(position) AS min_position
    FROM deals_list
    WHERE deleted_at IS NULL
    GROUP BY group_id
  ) first_pos ON first_pos.group_id = dl.group_id
    AND first_pos.min_position = dl.position
  WHERE dl.deleted_at IS NULL
  GROUP BY dl.group_id
) first_list ON first_list.group_id = et.lead_group_id
SET et.lead_list_id = first_list.list_id
WHERE et.lead_group_id IS NOT NULL
  AND et.lead_list_id IS NULL;

ALTER TABLE email_templates
  ADD UNIQUE KEY uk_email_templates_list_delay (lead_list_id, company_id, hour_delay);

ALTER TABLE scheduled_emails
  ADD COLUMN list_id INT NULL AFTER deal_id,
  ADD CONSTRAINT fk_scheduled_emails_list_id
    FOREIGN KEY (list_id) REFERENCES deals_list(id);

UPDATE scheduled_emails se
JOIN deals d ON d.id = se.deal_id
SET se.list_id = d.list_id
WHERE se.list_id IS NULL;
