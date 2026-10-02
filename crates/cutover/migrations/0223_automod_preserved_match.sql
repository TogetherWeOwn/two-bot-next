-- Preserve a pre-count match decision on the winning delivery claim, so a
-- same-revision gateway retry replays the IDs/reason-code decision after
-- unrelated traffic swept the mutable in-memory repeat history. The subject
-- is plain IDs only (guild/channel/message/author); no message content or
-- matched excerpt is persisted. NULL matched_filter means no inspection
-- decision is stored. `released` marks an explicit pre-count handoff: only a
-- released row replays its decision; a concurrently owned row stays InFlight.
ALTER TABLE automod_delivery_claims
  ADD COLUMN matched_filter TEXT,
  ADD COLUMN matched_guild_id TEXT,
  ADD COLUMN matched_channel_id TEXT,
  ADD COLUMN matched_message_id TEXT,
  ADD COLUMN matched_author_id TEXT,
  ADD COLUMN released BOOLEAN NOT NULL DEFAULT FALSE;
