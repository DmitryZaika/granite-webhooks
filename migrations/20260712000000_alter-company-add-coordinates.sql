-- Add latitude and longitude columns to company table for geolocation bias
ALTER TABLE company
    ADD COLUMN latitude DOUBLE NULL,
    ADD COLUMN longitude DOUBLE NULL;
