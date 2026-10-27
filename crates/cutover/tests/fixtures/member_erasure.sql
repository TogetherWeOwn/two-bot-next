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
      -- event_store is shared by the S1-S8 slices: one presence row per member
      -- here (S5 message/reaction columns ride along) plus three S8 rows per
      -- member below (actor-only / target-only / user-only) plus one S7
      -- invite row binding the member as inviter only, so the erasure suites
      -- expect five event_store rows per member. Voice columns stay NULL
      -- here; the backup seed covers the S4 voice, S5 message/reaction and
      -- S6 thread shapes.
      INSERT INTO event_store (event_type, guild_id, user_id, event_time, dedup_key,
        presence_status, activity_type, application_id, nick_changed, roles,
        channel_id, message_id)
        VALUES ('PRESENCE_UPDATE', g, u, '2026-10-01T00:00:00Z',
        'PRESENCE_UPDATE:' || g || ':' || u || ':2026-10-01T00:00:00Z',
        'online', 0, NULL, FALSE, '[]', 'c-' || k, 'm-' || k);
      INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, updated_at)
        VALUES (g, u, 1, u, now());
      INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
        VALUES (g, u, 15, 15, now());
      INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at)
        VALUES (g, '2026-Q4', u, 15, now());
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
      -- FK child of the idempotency row above: every self-role send writes an
      -- exchange row and nothing deletes them, so erasure/purge must delete
      -- the child first or the parent delete fails with 23503.
      INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
        VALUES ('exec-' || k, g, k, 'tok-' || k);
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
      INSERT INTO giveaways
        (guild_id, id, prize, channel_id, message_id, created_by, winners_count, ends_at,
         status, requirements_json, max_entries, draw_nonce, created_at, updated_at)
        VALUES (g, k, 'fixture', '999999999999999999', 'm-' || k, u, 1, now(),
         'open', '{}'::jsonb, 1, 'nonce-' || k, now(), now());
      INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at)
        VALUES (g, k, u, 1, now());
      INSERT INTO giveaway_draws
        (guild_id, giveaway_id, draw_no, drawn_at, eligible_count, eligible_hash, seed,
         winner_ids, reroll_kind)
        VALUES (g, k, 0, now(), 1, 'fixture', 'fixture', ('["' || u || '"]')::jsonb, NULL);
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
      -- FK child of the audit row above: insert_role_exchange writes one row
      -- per send, so erasure/purge must delete exchanges before the audit row.
      INSERT INTO self_role_exchanges (event_id, origin_generation, role_id, adding, compensating)
        VALUES (k, 1, '101', TRUE, FALSE);
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
      INSERT INTO voice_create_reservations (guild_id, user_id, created_at)
        VALUES (g, u, '2026-10-01T00:00:00Z');
      INSERT INTO voice_room_blocks (guild_id, room_channel_id, blocked_member_id)
        VALUES (g, 'v-' || k, u);
      INSERT INTO voice_join_grants (guild_id, room_channel_id, member_id)
        VALUES (g, 'v-' || k, u);
      INSERT INTO voice_owner_grants (guild_id, channel_id, member_id, revision, pending)
        VALUES (g, 'v-' || k, u, 'fixture-owner-revision', TRUE);
      INSERT INTO voice_logging_mention_members (guild_id, member_id) VALUES (g, u);
      INSERT INTO member_catchup_members (guild_id, member_id) VALUES (g, u);
      -- Starboard starrers are BIGINT snowflakes (one row per guild/message/user
      -- here, like every other member table): four rows total, one matching the
      -- erasure target, three survivors. The erasure predicate is
      -- guild_id = $1::bigint AND user_id = $2::bigint (cast the retained text
      -- binds, never the indexed columns); config.updated_by stays a
      -- retained-attribution allowlist and posts carry no member column.
      INSERT INTO starboard_starrers
        (guild_id, source_channel_id, source_message_id, user_id)
        VALUES (g::bigint, 155000000000000001, 156000000000000001, u::bigint);
      INSERT INTO voice_vote_kick_audit
        (guild_id, vote_id, event, room_id, initiator_id, target_id, outcome, occurred_at)
        VALUES (g, 'vk-' || k, 'vote_started', 'v-' || k, u, u, 'started', '2026-10-01T00:00:00Z');
      INSERT INTO retention_deletion_requests (request_id, guild_id, user_id)
        VALUES (k, g, u);
      -- Event voice presence is guild-scoped through the scheduled-event
      -- mirror (one event per member keeps the loop conflict-free).
      INSERT INTO scheduled_events
        (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at)
        VALUES (g, 'ev-' || k, 'fixture', '2026-11-01T20:00:00Z', NULL, NULL, 'scheduled', '2026-10-01T00:00:00Z');
      INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at, left_at)
        VALUES ('ev-' || k, '2026-11-01T20:00:00Z', u, '2026-11-01T20:00:00Z', NULL);
      -- Three S8 event_store rows per member, each binding the member in exactly
      -- one of actor_id, target_id and user_id (the others NULL) with distinct
      -- dedup_keys, so each erasure predicate branch is exercised
      -- independently; with the presence row above that is four rows per
      -- member. The THREAD_MEMBER_UPDATE shape stays covered by the backup
      -- seed, so no thread row is added here.
      -- One S7 event_store invite row per member, binding the member as
      -- inviter_id only (actor/target/user NULL) with a code hash, never the
      -- raw code; with the rows above that is five rows per member, so the
      -- inviter predicate branch is exercised independently too.
      INSERT INTO event_store
        (event_type, guild_id, stage_instance_id, privacy_level, interaction_id,
         interaction_type, application_id, command_id, audit_log_entry_id, action_type,
         actor_id, target_id, user_id, event_time, received_at, dedup_key)
        VALUES ('audit_log_entry', g, 'stage-actor-' || k, 1, 'interaction-actor-' || k,
          1, 'app-' || k, 'cmd-' || k, 'audit-actor-' || k, 1,
          u, NULL, NULL, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z',
          'AUDIT_LOG_ENTRY:' || g || ':audit-actor-' || k || ':2026-10-01T00:00:00Z');
      INSERT INTO event_store
        (event_type, guild_id, stage_instance_id, privacy_level, interaction_id,
         interaction_type, application_id, command_id, audit_log_entry_id, action_type,
         actor_id, target_id, user_id, event_time, received_at, dedup_key)
        VALUES ('audit_log_entry', g, 'stage-target-' || k, 1, 'interaction-target-' || k,
          1, 'app-' || k, 'cmd-' || k, 'audit-target-' || k, 1,
          NULL, u, NULL, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z',
          'AUDIT_LOG_ENTRY:' || g || ':audit-target-' || k || ':2026-10-01T00:00:00Z');
      INSERT INTO event_store
        (event_type, guild_id, stage_instance_id, privacy_level, interaction_id,
         interaction_type, application_id, command_id, audit_log_entry_id, action_type,
         actor_id, target_id, user_id, event_time, received_at, dedup_key)
        VALUES ('audit_log_entry', g, 'stage-user-' || k, 1, 'interaction-user-' || k,
          1, 'app-' || k, 'cmd-' || k, 'audit-user-' || k, 1,
          NULL, NULL, u, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z',
          'AUDIT_LOG_ENTRY:' || g || ':audit-user-' || k || ':2026-10-01T00:00:00Z');
      INSERT INTO event_store
        (event_type, guild_id, channel_id, invite_code_hash, inviter_id,
         actor_id, target_id, user_id, event_time, received_at, dedup_key)
        VALUES ('INVITE_CREATE', g, 'c-' || k,
          repeat(md5('invite-' || k), 2),
          u, NULL, NULL, NULL, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z',
          'INVITE_CREATE:' || g || ':c-' || k || ':' ||
            repeat(md5('invite-' || k), 2) ||
            ':2026-10-01T00:00:00Z');
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
