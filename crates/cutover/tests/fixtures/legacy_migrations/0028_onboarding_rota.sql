-- Derived onboarding milestones share the existing content-minimized fact log.
-- They are not raw scorecard streams and require no new stream heartbeat.
ALTER TABLE community_facts DROP CONSTRAINT community_facts_event_type_check;
ALTER TABLE community_facts ADD CONSTRAINT community_facts_event_type_check
  CHECK (event_type IN (
    'message_created', 'voice_session_started', 'voice_session_ended',
    'member_joined', 'event_attended', 'rules_accepted',
    'onboarding_rules_accepted', 'onboarding_prompt_shown',
    'onboarding_prompt_acted', 'onboarding_first_eligible_message',
    'onboarding_first_human_reply', 'onboarding_reply_latency',
    'onboarding_seven_day_return'
  ));

CREATE INDEX idx_community_facts_onboarding_member
  ON community_facts (guild_id, actor_id, event_type)
  WHERE event_type LIKE 'onboarding_%';
