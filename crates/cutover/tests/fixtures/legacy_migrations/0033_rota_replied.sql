-- First-message reply stops are independent of prompt exposure and latency.
-- This is operations evidence, not an eighth milestone or a coverage stream.
ALTER TABLE community_facts DROP CONSTRAINT community_facts_event_type_check;
ALTER TABLE community_facts ADD CONSTRAINT community_facts_event_type_check
  CHECK (event_type IN (
    'message_created', 'voice_session_started', 'voice_session_ended',
    'member_joined', 'event_attended', 'rules_accepted',
    'onboarding_rules_accepted', 'onboarding_prompt_shown',
    'onboarding_prompt_acted', 'onboarding_first_eligible_message',
    'onboarding_first_human_reply', 'onboarding_reply_latency',
    'onboarding_seven_day_return', 'welcome_rota_acknowledged',
    'welcome_rota_replied'
  ));
