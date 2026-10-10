-- Durable created/updated/cancelled receipt for website event intents.
-- Existing intents (announcements) stay NULL: their wire envelope renders
-- message_id, never an outcome. Event outcomes are a closed set, never
-- free text: replay must return the first result byte-identically, and a
-- create replayed after its key was registered must still read "created".
ALTER TABLE internal_idempotency
    ADD COLUMN outcome TEXT CHECK (outcome IN ('created', 'updated', 'cancelled'));
