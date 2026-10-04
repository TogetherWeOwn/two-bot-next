-- Two unrelated members in two guilds, across every erasure-plan table.
-- Fixed fake snowflakes only. Never load this fixture into a runtime database.
DO $$
DECLARE
  g TEXT;
  u TEXT;
  k TEXT;
  intent BIGINT;
BEGIN
  FOR g IN SELECT unnest(ARRAY['1545644954272137297', '1545644954272137298']) LOOP
    FOR u IN SELECT unnest(ARRAY['123456789012345678', '123456789012345679']) LOOP
      k := g || ':' || u;
      INSERT INTO members (guild_id, member_id) VALUES (g, u);
      INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
        VALUES ('member_join', u, g, now(), 'fixture', '{}', k);
      INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, updated_at)
        VALUES (g, u, 1, u, now());
      INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
        VALUES (g, u, 15, 15, now());
      INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at)
        VALUES (g, u, 'message', 15, now());
      INSERT INTO xp_cooldowns (guild_id, member_id, source, last_awarded_at)
        VALUES (g, u, 'message', now());
      INSERT INTO moderation_audit
        (request_id, guild_id, actor_id, action, target_id, reason, outcome, idempotency_key, metadata_json, created_at)
        VALUES (k, g, u, 'warn', u, 'fixture', 'ok', k, '{}', '2026-10-01T00:00:00Z');
      INSERT INTO moderation_idempotency
        (guild_id, idempotency_key, action, request_hash, state, claimed_at, completed_at, result_json)
        VALUES (g, k, 'warn', 'fixture', 'done', '2026-10-01T00:00:00Z', '2026-10-01T00:00:01Z', '{}');
      INSERT INTO moderation_warnings
        (id, guild_id, user_id, actor_id, reason, request_id, created_at)
        VALUES ('w-' || k, g, u, u, 'fixture', 'w-' || k, now());
      INSERT INTO moderation_scheduled_unbans
        (request_id, guild_id, user_id, execute_at, reason, state, created_at, completed_at)
        VALUES ('u-' || k, g, u, now(), 'fixture', 'completed', now(), now());
      INSERT INTO moderation_member_bans
        (request_id, guild_id, user_id, state, created_at)
        VALUES ('b-' || k, g, u, 'accepted', now());
      INSERT INTO automod_violations
        (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
        VALUES (g, u, 1, 'fixture', 'm-' || k, '2026-10-01T00:00:00Z');
      INSERT INTO automod_processed_messages (guild_id, message_id, user_id, processed_at)
        VALUES (g, 'm-' || k, u, '2026-10-01T00:00:00Z');
      INSERT INTO automod_delivery_claims
        (guild_id, message_id, delivery_kind, dry_run, request_hash, mutation_started,
         result_json, claimed_at, completed_at, counted, matched_filter, matched_guild_id,
         matched_channel_id, matched_message_id, matched_author_id)
        VALUES (g, 'm-' || k, 'create', FALSE, 'fixture', TRUE, '{}'::jsonb,
         '2026-10-01T00:00:00Z', '2026-10-01T00:00:01Z', TRUE, 'fixture', g, u, 'm-' || k, u);
      INSERT INTO sticky_messages
        (guild_id, channel_id, body, created_by, created_at, updated_by, updated_at)
        VALUES (g, u, 'fixture', u, now(), u, now());
      INSERT INTO automation_commands
        (guild_id, name, description, template, text_trigger, enabled, created_by, created_at, updated_by, updated_at)
        VALUES (g, 'fixture-' || u, 'fixture', 'fixture', NULL, TRUE, u, now(), u, now());
      INSERT INTO scheduled_messages
        (id, guild_id, channel_id, body, next_run_at, created_by, created_at, updated_by, updated_at)
        VALUES (k, g, u, 'fixture', '2026-10-01T00:00:00.000Z', u, '2026-10-01T00:00:00.000Z', u, '2026-10-01T00:00:00.000Z');
      INSERT INTO automation_audit_log (id, guild_id, actor_id, action, outcome, created_at)
        VALUES (k, g, u, 'sticky.create', 'ok', now());
      INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
        VALUES (g, '999999999999999999', u, 'going', now());
      INSERT INTO announcements_audit_log (id, guild_id, actor_id, action, outcome, created_at)
        VALUES (k, g, u, 'fixture', 'ok', now());
      INSERT INTO community_facts
        (guild_id, event_type, source_event_id, actor_id, occurred_at, source, classifier_version,
         classification, matched_rule, metadata, idempotency_key)
        VALUES (g, 'member_joined', k, u, '2026-10-01T00:00:00Z', 'fixture', 'fixture',
          'eligible_human', 'fixture', '{}', k);
      INSERT INTO community_scorecard_runs
        (guild_id, week_start, week_end, classifier_version, watermark, input_count, input_hash,
         idempotency_key, revision, run_status, coverage_state, evidence_state, scorecard_json,
         intervention_code, generated_at)
        VALUES (g, '2026-09-28T00:00:00Z', '2026-10-05T00:00:00Z', u, 1, 1, 'fixture', k,
          1, 'completed', 'complete', 'sufficient', '{"fixture_user_id":"' || u || '"}',
          'none', '2026-10-01T00:00:00Z');
      INSERT INTO lfg_posts (id, guild_id, channel_id, title, starts_at, status, created_by, created_at)
        VALUES (k, g, '999999999999999999', 'fixture', now(), 'open', u, now());
      INSERT INTO lfg_roles (lfg_id, role_key, label, slots, position)
        VALUES (k, 'tank', 'Tank', 2, 1);
      INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at)
        VALUES (k, u, 'tank', now());
      INSERT INTO feed_relays (id, guild_id, channel_id, kind, source, created_by, created_at, updated_at)
        VALUES (k, g, u, 'rss', 'https://example.invalid/feed', u, now(), now());
      INSERT INTO feed_deliveries (feed_id, item_key, nonce, state, first_seen_at)
        VALUES (k, 'fixture', 'fixture', 'delivered', now());
      INSERT INTO member_ranks (guild_id, member_id, updated_at)
        VALUES (g, u, '2026-10-01T00:00:00Z');
      INSERT INTO member_exclusions (guild_id, member_id, reason, updated_at)
        VALUES (g, u, 'raid', '2026-10-01T00:00:00Z');
      INSERT INTO join_risk_flags
        (event_id, guild_id, member_id, account_created_at, joined_at, source, score,
         reasons_json, bulk_join_window, flagged, created_at)
        VALUES (k, g, u, '2026-09-01T00:00:00Z', '2026-10-01T00:00:00Z', 'fixture', 3,
          '[]', false, true, '2026-10-01T00:00:00Z');
      INSERT INTO containment_events
        (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state,
         reason, created_at)
        VALUES (k, g, u, 'ban', u, 1, '2026-10-01T00:00:00Z', 'contain', 'fixture',
          '2026-10-01T00:00:00Z');
      INSERT INTO containment_incidents
        (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, result_json, started_at,
         completed_at)
        VALUES (k, g, u, k, 3, 'contained', '{}', '2026-10-01T00:00:00Z', '2026-10-01T00:00:01Z');
      INSERT INTO operational_audit_log
        (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id, metadata_json, created_at)
        VALUES (k, 'fixture', g, now(), u, u, '{}', now());
      INSERT INTO internal_idempotency
        (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id,
         response_code, http_status, resource_id, affected)
        VALUES (repeat(md5(k), 2), repeat(md5(k), 2), 'role.assign', repeat(md5(k), 2), 'completed',
          g, u, u, 'success', 200, u, 1) RETURNING intent_id INTO intent;
      INSERT INTO internal_action_log
        (intent_id, phase, caller_hash, action, guild_id, actor_id, target_id, response_code, http_status)
        VALUES (intent, 'terminal', repeat(md5(k), 2), 'role.assign', g, u, u, 'success', 200);
      INSERT INTO self_role_audit
        (event_id, guild_id, panel_id, member_id, source_id, source, operation, outcome,
         added_role_ids, removed_role_ids, created_at)
        VALUES (k, g, 'fixture-panel', u, 'fixture-source', 'button', 'assign', 'assigned',
          '[]', '[]', '2026-10-01T00:00:00Z');
      INSERT INTO self_role_panel_claims
        (guild_id, member_id, panel_id, claim_token, claim_generation, processing_expires_at,
         latest_event_id, target_committed)
        VALUES (g, u, 'fixture-panel', 'fixture-claim', 1, '2000-01-01T00:00:00Z', k, TRUE);
      INSERT INTO tickets (id, guild_id, channel_id, opener_id, claimed_by, status, created_at)
        VALUES ('t-' || k, g, 'c-' || k, u, u, 'closed', '2026-10-01T00:00:00Z');
      INSERT INTO ticket_transcripts
        (ticket_id, guild_id, channel_id, opener_id, claimed_by, content, message_count, created_at, purge_after)
        VALUES ('t-' || k, g, 'c-' || k, u, u, 'fixture', 1, '2026-10-01T00:00:00Z', '2026-12-30T00:00:00Z');
      INSERT INTO voice_rooms
        (guild_id, channel_id, creator_channel_id, owner_id, original_creator_id, name_seed, created_at)
        VALUES (g, 'v-' || k, 'vc-' || g, u, u, '7', '2026-10-01T00:00:00Z');
      INSERT INTO voice_logging_mention_members (guild_id, member_id) VALUES (g, u);
      INSERT INTO voice_vote_kick_audit
        (guild_id, vote_id, event, room_id, initiator_id, target_id, outcome, occurred_at)
        VALUES (g, 'vk-' || k, 'vote_started', 'v-' || k, u, u, 'started', '2026-10-01T00:00:00Z');
      -- Explicit exceptions are seeded too: erasure must not change safety policy.
      INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
        VALUES (g, 'fixture_' || u, to_jsonb(u), 1, u);
      INSERT INTO guild_settings_audit (guild_id, key, new_value, actor)
        VALUES (g, 'fixture_' || u, to_jsonb(u), u);
    END LOOP;
  END LOOP;
  INSERT INTO audit_kill_switch (id, engaged_at, engaged_by)
    VALUES (1, now(), '123456789012345678');
END $$;
