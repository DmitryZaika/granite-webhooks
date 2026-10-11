-- Explicit native e-signature flag. NULL keeps the legacy behavior
-- (eligibility inferred from a nonblank signwell_api_key); backfill at cutover only.
ALTER TABLE company
    ADD COLUMN signature_enabled TINYINT(1) NULL;
