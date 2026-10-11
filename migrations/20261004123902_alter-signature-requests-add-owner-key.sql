-- Tenant ownership key referenced by the native signature tables' foreign keys
ALTER TABLE signature_requests
    ADD UNIQUE KEY uq_signature_requests_owner (id, company_id);
