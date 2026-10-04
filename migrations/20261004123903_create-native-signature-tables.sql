-- Native (first-party) e-signature storage replacing SignWell.
-- Mirrors NATIVE_SIGNATURE_TEST_DDL in general_datebase tests/nativeSignatureDatabase.ts.

CREATE TABLE native_signature_documents (
    request_id INT NOT NULL PRIMARY KEY,
    company_id INT NOT NULL,
    document_id CHAR(36) NOT NULL,
    prepared_object_key VARCHAR(1024) NOT NULL,
    prepared_sha256 CHAR(64) NOT NULL,
    document_version CHAR(64) NOT NULL,
    token_hash CHAR(64) NOT NULL,
    fields_json JSON NOT NULL,
    settings_json JSON NOT NULL,
    completed_object_key VARCHAR(1024) NULL,
    completed_sha256 CHAR(64) NULL,
    expires_at DATETIME(3) NULL,
    revoked_at DATETIME(3) NULL,
    sent_at DATETIME(3) NULL,
    viewed_at DATETIME(3) NULL,
    signed_at DATETIME(3) NULL,
    UNIQUE KEY uq_native_signature_documents_document (document_id),
    UNIQUE KEY uq_native_signature_documents_token (token_hash),
    INDEX idx_native_signature_documents_expiration (expires_at, request_id),
    CONSTRAINT fk_native_signature_documents_owner
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_sessions (
    session_hash CHAR(64) NOT NULL PRIMARY KEY,
    request_id INT NOT NULL,
    company_id INT NOT NULL,
    invitation_token_hash CHAR(64) NOT NULL,
    expires_at DATETIME(3) NOT NULL,
    reviewed_at DATETIME(3) NULL,
    created_at DATETIME(3) NOT NULL,
    INDEX idx_native_signature_sessions_expiry (expires_at),
    CONSTRAINT fk_native_signature_sessions_owner
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_finalizations (
    request_id INT NOT NULL PRIMARY KEY,
    company_id INT NOT NULL,
    idempotency_key VARCHAR(64) NOT NULL,
    submission_sha256 CHAR(64) NOT NULL,
    submission_json JSON NOT NULL,
    audit_json JSON NOT NULL,
    signed_at DATETIME(3) NOT NULL,
    state ENUM('rendering', 'retryable', 'completed') NOT NULL,
    lease_owner CHAR(36) NULL,
    lease_until DATETIME(3) NULL,
    completed_object_key VARCHAR(1024) NULL,
    completed_sha256 CHAR(64) NULL,
    evidence_object_key VARCHAR(1024) NULL,
    CONSTRAINT fk_native_signature_finalizations_owner
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_audit (
    id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    request_id INT NOT NULL,
    company_id INT NOT NULL,
    event_key VARCHAR(100) NOT NULL,
    event_type VARCHAR(32) NOT NULL,
    evidence_json JSON NOT NULL,
    created_at DATETIME(3) NOT NULL,
    UNIQUE KEY uq_native_signature_audit_event (request_id, event_key),
    CONSTRAINT fk_native_signature_audit_owner
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_deliveries (
    id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    created_by_user_id INT NOT NULL,
    logical_send_hash CHAR(64) NOT NULL,
    payload_sha256 CHAR(64) NOT NULL,
    state ENUM('prepared', 'sending', 'uncertain', 'accepted', 'rejected', 'recorded') NOT NULL,
    lease_owner CHAR(36) NULL,
    lease_until DATETIME(3) NULL,
    encrypted_mail LONGTEXT NULL,
    accepted_at DATETIME(3) NULL,
    email_id INT NULL,
    created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    UNIQUE KEY uq_native_signature_deliveries_send (company_id, created_by_user_id, logical_send_hash),
    UNIQUE KEY uq_native_signature_deliveries_owner (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_delivery_items (
    delivery_id BIGINT NOT NULL,
    company_id INT NOT NULL,
    request_id INT NOT NULL,
    attachment_index INT NOT NULL,
    PRIMARY KEY (delivery_id, attachment_index),
    UNIQUE KEY uq_native_signature_delivery_items_request (request_id),
    CONSTRAINT fk_native_signature_delivery_items_delivery
        FOREIGN KEY (delivery_id, company_id) REFERENCES native_signature_deliveries (id, company_id),
    CONSTRAINT fk_native_signature_delivery_items_request
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE native_signature_jobs (
    id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    request_id INT NOT NULL,
    company_id INT NOT NULL,
    job_key VARCHAR(32) NOT NULL,
    kind ENUM('reminder', 'notification') NOT NULL,
    due_at DATETIME(3) NOT NULL,
    state ENUM('pending', 'sending', 'retry', 'sent', 'failed', 'uncertain', 'blocked', 'cancelled') NOT NULL,
    attempts INT NOT NULL DEFAULT 0,
    lease_owner CHAR(36) NULL,
    lease_until DATETIME(3) NULL,
    sent_at DATETIME(3) NULL,
    message_id VARCHAR(255) NULL,
    UNIQUE KEY uq_native_signature_jobs_key (request_id, company_id, job_key),
    INDEX idx_native_signature_jobs_due (state, due_at),
    CONSTRAINT fk_native_signature_jobs_owner
        FOREIGN KEY (request_id, company_id) REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
