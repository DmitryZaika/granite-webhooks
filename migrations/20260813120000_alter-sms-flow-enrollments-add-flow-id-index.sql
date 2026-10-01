ALTER TABLE sms_flow_enrollments ADD INDEX idx_sms_flow_enrollments_flow (flow_id, status);
