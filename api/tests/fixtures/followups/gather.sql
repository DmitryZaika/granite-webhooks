UPDATE customers SET remodal_type = 'Kitchen', project_size = '45', details = 'Looking at quartz countertops for kitchen island',
  your_message = 'Please call me after 5pm, we want white quartz', notes = 'Met at home show', city = 'Indianapolis', state = 'IN',
  sink = 'Undermount', backsplash = '4 inch', contact_time = 'Evenings', created_date = '2026-03-01 10:00:00'
 WHERE id = 1000;
INSERT INTO cloudtalk_sms (id, sender, recipient, text, direction, status, sender_user_id, company_id, recipient10, created_date, is_automated) VALUES
  (9900, 3175550100, 3175550999, 'Hi, we are still thinking about the white quartz. Can you send the price?', 'inbound', 'received', NULL, 100, '3175550999', '2026-09-20 15:00:00', 0),
  (9901, 3175550999, 3175550100, 'Sure, I will send the quote today', 'outbound', 'sent', 100, 100, '3175550100', '2026-09-20 16:00:00', 0),
  (9902, 3175550100, 3175550999, 'Thanks! we will decide next month', 'inbound', 'received', NULL, 100, '3175550999', '2026-09-22 15:30:00', 0);
INSERT INTO ringcentral_sms (id, sender, recipient, text, direction, status, sender_user_id, company_id, created_date) VALUES
  (9905, 13175550101, 13175550999, 'Old RingCentral text from the second phone', 'inbound', 'received', NULL, 100, '2026-06-01 12:00:00');
INSERT INTO emails (id, subject, body, sent_at, message_id, thread_id, sender_user_id, receiver_user_id, sender_email, receiver_email, company_id, is_draft) VALUES
  (9910, 'Kitchen quote', '<p>Could you <b>quote</b> the island?</p>', '2026-09-10 10:00:00', '<g9910@x>', 'dddddddd-0000-4000-8000-000000009910', NULL, 100, 'Alice Lead <alice@example.com>', 'rep@acme.test', 100, 0),
  (9911, 'Re: Kitchen quote', 'Attached is the quote.', '2026-09-11 10:00:00', '<g9911@x>', 'dddddddd-0000-4000-8000-000000009910', 100, NULL, 'Rita Rep <rep@acme.test>', 'alice@example.com', 100, 0);
INSERT INTO deal_notes (id, deal_id, company_id, content, is_pinned, created_at, created_by) VALUES
  (9920, 8002, 100, 'Customer said check back in a month', 0, '2026-10-01 13:00:00', 'Rita Rep');
INSERT INTO deal_activities (id, deal_id, company_id, name, deadline, priority, is_completed, created_at, created_by) VALUES
  (9925, 8002, 100, 'Follow up   on quote', '2026-11-20 09:00:00', 'medium', 0, '2026-10-02 11:00:00', 'Rita Rep'),
  (9926, 8002, 100, 'Overdue call', '2026-09-01 09:00:00', 'high', 0, '2026-08-20 11:00:00', 'Rita Rep');
INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, position) VALUES
  (9930, 100, 100, 'My check-in', 'Hi {name}, just checking in on your project!', 0);
INSERT INTO email_templates (id, template_name, template_subject, template_body, company_id, user_id) VALUES
  (9931, 'My quote follow-up', 'Your quote', '<p>Hi there, <i>any</i> questions on the quote?</p>', 100, 100);
INSERT INTO instructions (id, title, rich_text, company_id) VALUES
  (9935, 'Tone', '<p>Be <b>friendly</b>.</p>', 100),
  (9936, 'AI follow-up instruction', 'send messages with 2 day gap', 100);
INSERT INTO ai_followup_style_preferences (user_id, company_id, preferences_json, sample_count) VALUES
  (100, 100, '{"smsLength":"SHORT","emailLength":null,"questionCount":"ONE_MAX","exclamation":null,"customerName":null,"greeting":"ADD","link":null}', 5);
