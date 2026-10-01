-- Rota operations evidence shares the existing pseudonymous fact log.
-- An acknowledgement is not a human reply or an activity/coverage stream.
ALTER TABLE community_facts DROP CONSTRAINT community_facts_event_type_check;
ALTER TABLE community_facts ADD CONSTRAINT community_facts_event_type_check
  CHECK (event_type IN (
    'message_created', 'voice_session_started', 'voice_session_ended',
    'member_joined', 'event_attended', 'rules_accepted',
    'onboarding_rules_accepted', 'onboarding_prompt_shown',
    'onboarding_prompt_acted', 'onboarding_first_eligible_message',
    'onboarding_first_human_reply', 'onboarding_reply_latency',
    'onboarding_seven_day_return', 'welcome_rota_acknowledged'
  ));

CREATE INDEX idx_community_facts_rota_due
  ON community_facts (guild_id, occurred_at, id)
  WHERE event_type = 'onboarding_first_eligible_message' AND classification = 'eligible_human';
