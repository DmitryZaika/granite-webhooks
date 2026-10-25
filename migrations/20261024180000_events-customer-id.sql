-- The customer an event is for. The title is the event's own name: it starts
-- as the customer's name and people may change it. Events saved before this
-- held the customer id as their title: link that customer and name the event
-- after them.
ALTER TABLE events
    ADD COLUMN customer_id INT NULL AFTER sale_id,
    ADD CONSTRAINT fk_events_customer
        FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE SET NULL;

-- IF keeps CAST off other titles (strict mode fails an UPDATE on a bad cast).
UPDATE events e
  JOIN users u ON u.id = e.created_user_id
  JOIN customers c ON c.company_id = u.company_id
   AND c.id = IF(TRIM(e.title) REGEXP '^[0-9]{1,10}$', CAST(TRIM(e.title) AS UNSIGNED), NULL)
   SET e.customer_id = c.id,
       e.title = COALESCE(NULLIF(TRIM(c.name), ''), e.title);
