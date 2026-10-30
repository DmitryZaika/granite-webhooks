-- Every agent id and number a user has texted with, on any provider, so
-- their texts follow them to the provider they text through now even after
-- the field is cleared (`routes::sms::moves`). Filled from the users' fields,
-- the agent ids and numbers of the texts they sent, and their direct
-- Granite Phone lines; saves add to it.
CREATE TABLE user_sms_identities (
  company_id INT NOT NULL,
  user_id INT NOT NULL,
  provider ENUM('cloudtalk', 'ringcentral', 'telnyx') NOT NULL,
  kind ENUM('agent', 'line') NOT NULL,
  value VARCHAR(64) NOT NULL,
  created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  PRIMARY KEY (company_id, user_id, provider, kind, value),
  KEY idx_user_sms_identities_user (user_id)
);

INSERT IGNORE INTO user_sms_identities (company_id, user_id, provider, kind, value)
SELECT company_id, id, 'cloudtalk', 'agent', TRIM(cloudtalk_agent_id) FROM users
 WHERE company_id IS NOT NULL AND COALESCE(TRIM(cloudtalk_agent_id), '') <> ''
UNION ALL
SELECT company_id, id, 'ringcentral', 'agent', TRIM(ringcentral_extension_id) FROM users
 WHERE company_id IS NOT NULL AND COALESCE(TRIM(ringcentral_extension_id), '') <> ''
UNION ALL
SELECT company_id, id, 'cloudtalk', 'line', RIGHT(REGEXP_REPLACE(cloudtalk_phone_number, '[^0-9]', ''), 10)
  FROM users
 WHERE company_id IS NOT NULL
   AND LENGTH(REGEXP_REPLACE(COALESCE(cloudtalk_phone_number, ''), '[^0-9]', '')) >= 10
UNION ALL
SELECT company_id, id, 'ringcentral', 'line', RIGHT(REGEXP_REPLACE(ringcentral_phone_number, '[^0-9]', ''), 10)
  FROM users
 WHERE company_id IS NOT NULL
   AND LENGTH(REGEXP_REPLACE(COALESCE(ringcentral_phone_number, ''), '[^0-9]', '')) >= 10;

-- The agent ids and numbers of the texts each user sent from the CRM.
INSERT IGNORE INTO user_sms_identities (company_id, user_id, provider, kind, value)
SELECT DISTINCT s.company_id, s.sender_user_id, 'cloudtalk', 'agent', TRIM(s.agent)
  FROM cloudtalk_sms s JOIN users u ON u.id = s.sender_user_id AND u.company_id = s.company_id
 WHERE COALESCE(TRIM(s.agent), '') <> ''
UNION ALL
SELECT DISTINCT s.company_id, s.sender_user_id, 'ringcentral', 'agent', TRIM(s.agent)
  FROM ringcentral_sms s JOIN users u ON u.id = s.sender_user_id AND u.company_id = s.company_id
 WHERE COALESCE(TRIM(s.agent), '') <> ''
   AND COALESCE(s.idempotency_key, '') NOT REGEXP '^(cloudtalk|ringcentral|telnyx):[0-9]+$'
UNION ALL
SELECT DISTINCT s.company_id, s.sender_user_id, 'cloudtalk', 'line', s.sender10
  FROM cloudtalk_sms s JOIN users u ON u.id = s.sender_user_id AND u.company_id = s.company_id
 WHERE s.direction = 'outbound' AND LENGTH(s.sender10) = 10
UNION ALL
SELECT DISTINCT s.company_id, s.sender_user_id, 'ringcentral', 'line', s.sender10
  FROM ringcentral_sms s JOIN users u ON u.id = s.sender_user_id AND u.company_id = s.company_id
 WHERE s.direction = 'outbound' AND LENGTH(s.sender10) = 10
   AND COALESCE(s.idempotency_key, '') NOT REGEXP '^(cloudtalk|ringcentral|telnyx):[0-9]+$';

-- Granite Phone: a user's texts there carry their user id; a direct line is
-- theirs alone (a shared or main line is not).
INSERT IGNORE INTO user_sms_identities (company_id, user_id, provider, kind, value)
SELECT DISTINCT n.company_id, nu.user_id, 'telnyx', 'agent', CAST(nu.user_id AS CHAR)
  FROM telnyx_number_users nu
  JOIN telnyx_numbers n ON n.id = nu.number_id
  JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id
 WHERE nu.can_call_from <> 0
UNION ALL
SELECT DISTINCT n.company_id, nu.user_id, 'telnyx', 'line', n.last10
  FROM telnyx_number_users nu
  JOIN telnyx_numbers n ON n.id = nu.number_id
  JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id
 WHERE nu.can_call_from <> 0 AND n.kind = 'direct' AND LENGTH(n.last10) = 10;
