-- Minimal operator accountability. Deliberately no target, guild, content,
-- counts, request identifier or hashed subject (a hash is still linkable).
CREATE TABLE member_erasure_audit (
    actor TEXT NOT NULL CHECK (length(actor) BETWEEN 1 AND 128),
    erased_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
