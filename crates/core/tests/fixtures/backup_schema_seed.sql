-- Real cutover-migrated schema only. Every active DUMP_TABLES table has data.
-- Explicit, gapped serial IDs intentionally leave sequences behind their rows.
-- Recovery leases, fences and security/delivery dedupe are durable backup data.
INSERT INTO events
    (id, event_type, member_id, guild_id, occurred_at, recorded_at, source, metadata, idempotency_key)
VALUES
    (41, 'member_join', '100000000000000002', '100000000000000001',
     '2026-08-01T12:00:00.123456+02:00', '2026-08-01T10:00:01.654321Z',
     'gateway', '{"channelId":"100000000000000003","note":"雪 | NULL"}', 'backup:event:join'),
    (107, 'onboarding_prompted', '100000000000000004', '100000000000000001',
     '2026-08-02T10:00:00Z', '2026-08-02T10:00:01Z', 'onboarding', NULL, 'backup:event:prompt');

INSERT INTO members
    (guild_id, member_id, joined_at, join_source, first_message_at, third_message_at,
     first_voice_at, last_active_at, left_at, inactive_flagged_at, gate_cleared_at, is_bot)
VALUES
    ('100000000000000001', '100000000000000002', '2026-08-01T10:00:00.123456Z',
     'invite:backup', '2026-08-01T10:01:00Z', '2026-08-01T10:03:00Z',
     '2026-08-01T10:05:00Z', '2026-08-02T10:00:00Z', '2026-08-03T10:00:00Z',
     '2026-08-04T10:00:00Z', '2026-08-01T10:00:30Z', FALSE),
    ('100000000000000001', '100000000000000004', NULL, 'NULL', NULL, NULL,
     NULL, NULL, NULL, NULL, NULL, TRUE);

INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
VALUES ('100000000000000001', 'backup-invite', 7, '100000000000000002',
        '100000000000000003', '2026-08-01T09:00:00Z');

INSERT INTO operational_audit_log
    (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id,
     source_channel_id, destination_channel_id, message_id, action, metadata_json, created_at,
     mirror_channel_id, delivery_state, delivery_attempts, delivery_attempted_at,
     delivery_last_error, delivery_lease_until, mirrored_at, delivery_nonce,
     mirror_message_id, delivery_search_before, delivery_claim_token, mirror_checked_at,
     delivery_generation, delivery_accepted_at, delivery_deferred_until, delivery_yielded_at)
VALUES
    ('backup:audit:lease', 'message_delete', '100000000000000001', '2026-08-01T10:00:00Z',
     '100000000000000002', '100000000000000004', '100000000000000003',
     '100000000000000005', '100000000000000006', 'delete', '{"count":1}',
     '2026-08-01T10:00:01Z', '100000000000000007', 'pending', 3, '2026-08-01T10:00:02Z',
     'transient', '2026-08-01T10:01:02Z', '2026-08-01T10:00:03Z', 'backup-delivery-nonce',
     '100000000000000008', '100000000000000009', 'backup-audit-claim', '2026-08-01T10:00:04Z',
     19, '2026-08-01T10:00:05Z', '2026-08-01T10:02:00Z', '2026-08-01T10:00:06Z');

INSERT INTO moderation_audit
    (request_id, guild_id, actor_id, action, target_id, channel_id, reason, outcome,
     idempotency_key, metadata_json, created_at)
VALUES ('backup:moderation:audit', '100000000000000001', '100000000000000002',
        'lockdown', '100000000000000004', '100000000000000003', 'backup recovery', 'ok',
        'backup:moderation:key', '{"priorExists":true}', '2026-08-01T10:00:00.000Z');
INSERT INTO moderation_lockdowns
    (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at, recovery_generation)
VALUES ('100000000000000003', '100000000000000001', '9007199254740993', '2048',
        TRUE, 'backup lockdown', '2026-08-01T10:00:00.000Z', 'backup-recovery-generation');
INSERT INTO moderation_idempotency
    (guild_id, idempotency_key, action, request_hash, state, outcome, result_json,
     claimed_at, completed_at, claim_token)
VALUES ('100000000000000001', 'backup:moderation:key', 'lockdown', 'backup-request-hash',
        'completed', 'ok', '{"channelId":"100000000000000003"}',
        '2026-08-01T10:00:00.000Z', '2026-08-01T10:00:01.000Z', 'backup-moderation-claim');
INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
VALUES ('100000000000000003', '100000000000000001', 'backup:moderation:key', 'backup-channel-claim');
INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at)
VALUES ('backup:warning', '100000000000000001', '100000000000000005', '100000000000000002',
        'backup warning', 'backup:warning:request', '2026-08-01T10:00:00.000Z');
-- Gapped generations, and a retry ticket past both: restore must resume the
-- shared ownership sequence beyond every restored allocation from it.
INSERT INTO moderation_member_bans
    (request_id, guild_id, user_id, generation, state, created_at, completed_at)
VALUES ('backup:ban:temp', '100000000000000001', '100000000000000005', 4, 'accepted',
        '2026-08-01T10:00:00.000Z', '2026-08-01T10:00:01.000Z'),
       ('backup:ban:refused', '100000000000000001', '100000000000000006', 7, 'rejected',
        '2026-08-01T10:00:02.000Z', '2026-08-01T10:00:03.000Z');
-- Terminal: restore quarantines only staged/pending/running expiries.
INSERT INTO moderation_scheduled_unbans
    (request_id, guild_id, user_id, execute_at, reason, state, created_at, completed_at,
     claimed_at, claim_token, dispatch_uncertain, retry_generation)
VALUES ('backup:ban:temp', '100000000000000001', '100000000000000005',
        '2026-08-02T10:00:00.000Z', 'backup tempban', 'done', '2026-08-01T10:00:00.000Z',
        '2026-08-02T10:00:02.000Z', '2026-08-02T10:00:01.000Z', 'backup-unban-claim', FALSE, 12);

INSERT INTO containment_events
    (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason,
     created_at)
VALUES ('100000000000000101', '100000000000000001', '100000000000000005', 'channel.delete',
        '100000000000000003', 3, '2026-08-01T09:00:00.000Z', 'contain',
        E'counted toward destructive-action heat | NULL\\n雪 \\"quote\\"',
        '2026-08-01T09:00:00.120Z'),
       ('100000000000000102', '100000000000000001', NULL, 'webhook.create', NULL, 1,
        '2026-08-01T09:00:01.000Z', 'ignored', 'audit entry has no executor; refusing to guess',
        '2026-08-01T09:00:01.000Z');
INSERT INTO containment_incidents
    (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, result_json, started_at,
     cooldown_until, completed_at)
VALUES ('100000000000000101', '100000000000000001', '100000000000000005', '100000000000000101',
        6, 'uncertain', E'{"removedRoleIds":["100000000000000004"],"failure":"timeout | NULL\\n雪"}',
        '2026-08-01T09:00:00.200Z', '2026-08-01T09:01:00.200Z', '2026-08-01T09:00:02.000Z');

INSERT INTO join_risk_flags
    (event_id, guild_id, member_id, account_created_at, joined_at, source, score, reasons_json,
     bulk_join_window, flagged, created_at)
VALUES ('backup:risk:flagged', '100000000000000001', '100000000000000005',
        '2026-07-31T23:59:00.000Z', '2026-08-01T08:00:00.000Z', 'gateway', 85,
        E'["new_account","bulk_join | NULL\\n雪 \\"quote\\""]', TRUE, TRUE,
        '2026-08-01T08:00:00.123Z'),
       ('backup:risk:clear', '100000000000000001', '100000000000000002',
        '2020-01-01T00:00:00.000Z', '2026-08-01T08:05:00.000Z', 'gateway', 0, '[]', FALSE,
        FALSE, '2026-08-01T08:05:00.000Z');

INSERT INTO sticky_messages
    (guild_id, channel_id, body, debounce_seconds, enabled, last_message_id, last_posted_at,
     created_by, created_at, updated_by, updated_at, claim_token, claimed_at)
VALUES ('100000000000000001', '100000000000000003', E'Backup sticky | NULL\n雪 "quote"',
        17, FALSE, '100000000000000006', '2026-08-01T10:00:00.123456Z',
        '100000000000000002', '2026-08-01T09:00:00Z', '100000000000000004',
        '2026-08-01T10:00:00Z', 'backup-sticky-claim', '2026-08-01T10:00:01Z');
INSERT INTO automation_commands
    (guild_id, name, description, template, text_trigger, enabled,
     created_by, created_at, updated_by, updated_at)
VALUES ('100000000000000001', 'backup-custom', 'Backup custom',
        E'Backup custom {user} | NULL\n雪 "quote"', '!backup', TRUE,
        '100000000000000002', '2026-08-01T09:00:00Z', '100000000000000004',
        '2026-08-01T10:00:00Z');
INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
VALUES ('backup:automation:audit', '100000000000000001', NULL, 'sticky.run',
        '100000000000000003', 'post_failed', 'transient', '2026-08-01T10:00:00Z');

INSERT INTO scheduled_messages
    (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled, last_run_at,
     last_message_id, created_by, created_at, updated_by, updated_at, claim_token, claimed_at,
     occurrence_nonce)
VALUES ('backup:scheduled:recurring', '100000000000000001', '100000000000000003',
        E'Backup schedule | NULL\n雪 "quote"', '2026-08-02T10:00:00.000Z', 31536000, TRUE,
        '2026-08-01T10:00:00.000Z', '100000000000000006', '100000000000000002',
        '2026-08-01T09:00:00.000Z', '100000000000000004', '2026-08-01T10:00:00.000Z',
        'backup-scheduled-claim', '2026-08-01T10:00:01.000Z', 'backup-occurrence'),
       ('backup:scheduled:one-shot', '100000000000000001', '100000000000000003',
        'One shot', '2026-08-03T10:00:00.000Z', NULL, FALSE, NULL, NULL,
        '100000000000000002', '2026-08-01T09:00:00.000Z', '100000000000000002',
        '2026-08-01T09:00:00.000Z', NULL, NULL, NULL);

INSERT INTO tickets
    (id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at,
     closed_at)
VALUES ('backup:ticket:closed', '100000000000000001', '100000000000000007',
        '100000000000000002', '100000000000000004', 'closed', '2026-08-01T09:00:00.000Z',
        '2026-08-01T09:30:00.000Z', '2026-08-01T09:31:00.000Z'),
       ('backup:ticket:creating', '100000000000000001', NULL, '100000000000000005', NULL,
        'creating', '2026-08-01T10:00:00.000Z', NULL, NULL);
INSERT INTO ticket_transcripts
    (ticket_id, guild_id, channel_id, opener_id, claimed_by, content, message_count, created_at,
     purge_after)
VALUES ('backup:ticket:closed', '100000000000000001', '100000000000000007',
        '100000000000000002', '100000000000000004', E'[09:00] opener: help | NULL\n雪 "quote"',
        2, '2026-08-01T09:31:00.000Z', '2026-10-30T09:31:00.000Z');

INSERT INTO automod_violations
    (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
VALUES ('100000000000000001', '100000000000000002', 2, 'invite_links',
        '100000000000000020', '2026-08-01T10:02:00.000Z');
INSERT INTO automod_processed_messages (guild_id, message_id, user_id, processed_at)
VALUES ('100000000000000001', '100000000000000019', '100000000000000002',
        '2026-08-01T10:01:00.000Z'),
       ('100000000000000001', '100000000000000020', '100000000000000002',
        '2026-08-01T10:02:00.000Z');
-- A started, uncompleted mutation is a replay guard: restore must keep it.
INSERT INTO automod_delivery_claims
    (guild_id, message_id, delivery_kind, dry_run, request_hash, claim_token,
     mutation_started, result_json, claimed_at, completed_at, counted, matched_filter,
     matched_guild_id, matched_channel_id, matched_message_id, matched_author_id, released)
VALUES ('100000000000000001', '100000000000000020', 'create', FALSE, repeat('7', 64),
        'backup-automod-claim', TRUE, NULL, '2026-08-01T10:02:00.123456Z', NULL, TRUE,
        'invite_links', '100000000000000001', '100000000000000003', '100000000000000020',
        '100000000000000002', FALSE),
       ('100000000000000001', '100000000000000021', 'update', TRUE, repeat('8', 64),
        'backup-automod-dry-run', FALSE, '{"outcome":"clean","note":"雪 | NULL"}',
        '2026-08-01T10:03:00Z', '2026-08-01T10:03:01Z', FALSE, NULL, NULL, NULL, NULL,
        NULL, FALSE);

INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
VALUES ('100000000000000001', '100000000000000002', 9007199254740991,
        31, 60, 9007199254740900, '2026-08-01T10:00:00Z');
-- Excluded: must be cleared by successful restore, preserved on failed restore.
INSERT INTO xp_cooldowns (guild_id, member_id, source, last_awarded_at)
VALUES ('100000000000000001', '100000000000000002', 'message', '2026-08-01T10:00:00Z');
INSERT INTO xp_awards (id, guild_id, member_id, source, xp, occurred_at, channel_id)
VALUES (43, '100000000000000001', '100000000000000002', 'voice', 60,
        '2026-08-01T10:00:00Z', '100000000000000003');
INSERT INTO level_role_rewards (guild_id, level, role_id)
VALUES ('100000000000000001', 17, '100000000000000010');
INSERT INTO level_import_runs
    (id, guild_id, source, source_rows, unique_members, inserted, updated, unchanged,
     duplicate_rows, total_imported_xp, imported_at)
VALUES (53, '100000000000000001', 'mee6', 7, 6, 2, 3, 1, 1,
        9007199254740900, '2026-08-01T10:00:00Z');

INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
VALUES ('100000000000000001', '100000000000000011', '100000000000000002',
        'interested', '2026-08-01T10:00:00Z');
INSERT INTO announcements_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
VALUES ('backup:announcement:audit', '100000000000000001', '100000000000000002',
        'rsvp.set', '100000000000000011', 'ok', 'backup RSVP', '2026-08-01T10:00:00Z');
INSERT INTO community_facts
    (id, guild_id, event_type, source_event_id, actor_id, occurred_at, recorded_at,
     source, classifier_version, classification, matched_rule, metadata, idempotency_key)
VALUES (61, '100000000000000001', 'event_attended', '100000000000000011',
        '100000000000000002', '2026-08-01T10:00:00.000Z', '2026-08-01T10:00:01.000Z',
        'host_checkin', 'classifier-v1', 'eligible_human', 'default',
        '{"attended":true}', 'backup:fact:attended');

INSERT INTO lfg_posts
    (id, guild_id, channel_id, message_id, title, starts_at, status, created_by, created_at, closed_at)
VALUES ('backup:lfg', '100000000000000001', '100000000000000003', '100000000000000006',
        'Backup raid 雪', '2026-08-02T18:00:00+02:00', 'closed',
        '100000000000000002', '2026-08-01T10:00:00Z', '2026-08-02T19:00:00Z');
INSERT INTO lfg_roles (lfg_id, role_key, label, slots, position)
VALUES ('backup:lfg', 'tank', 'Tank', 2, 1), ('backup:lfg', 'healer', 'Healer', 3, 2);
INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at)
VALUES ('backup:lfg', '100000000000000002', 'tank', '2026-08-01T10:00:00Z'),
       ('backup:lfg', '100000000000000004', 'healer', '2026-08-01T10:01:00Z');

INSERT INTO feed_relays
    (id, guild_id, channel_id, kind, source, enabled, last_checked_at, created_by, created_at, updated_at)
VALUES ('backup:feed', '100000000000000001', '100000000000000003', 'rss',
        'https://example.invalid/feed.xml', FALSE, '2026-08-01T10:00:00Z',
        '100000000000000002', '2026-08-01T09:00:00Z', '2026-08-01T10:00:00Z');
INSERT INTO feed_deliveries
    (feed_id, item_key, nonce, state, message_id, first_seen_at, delivered_at, claim_token, claimed_at)
VALUES ('backup:feed', 'backup:item:pending', 'backup-feed-nonce-pending', 'pending', NULL,
        '2026-08-01T10:00:00Z', NULL, 'backup-feed-claim', '2026-08-01T10:00:01Z'),
       ('backup:feed', 'backup:item:delivered', 'backup-feed-nonce-delivered', 'delivered',
        '100000000000000006', '2026-08-01T09:00:00Z', '2026-08-01T09:00:01Z', NULL, NULL);

-- These two tables contain migration baselines; replace/update rather than duplicate them.
UPDATE web_contract_meta SET contract_version = '1.1-backup', guild_id = '100000000000000001';
UPDATE rank_ladder SET role_id = '100000000000000010' || rank_order::text;
INSERT INTO guild_counters
    (guild_id, human_member_count, human_member_count_at, online_count, online_count_at)
VALUES ('100000000000000001', 127, '2026-08-01T10:00:00.000Z', 17, '2026-08-01T10:01:00.000Z');
INSERT INTO rank_snapshots (guild_id, rank_key, member_count, holders_count, snapshot_at)
VALUES ('100000000000000001', 'legend', 3, 7, '2026-08-01T10:00:00.000Z');
INSERT INTO member_ranks (guild_id, member_id, rank_key, updated_at)
VALUES ('100000000000000001', '100000000000000002', 'legend', '2026-08-01T10:00:00.000Z'),
       ('100000000000000001', '100000000000000004', NULL, '2026-08-01T10:01:00.000Z');
INSERT INTO scheduled_events
    (guild_id, event_id, name, starts_at, channel_id, description, status, updated_at)
VALUES ('100000000000000001', '100000000000000011', 'Backup event', '2026-08-02T18:00:00.000Z',
        '100000000000000003', E'Event | NULL\n雪', 'scheduled', '2026-08-01T10:00:00.000Z');
INSERT INTO counter_snapshots
    (guild_id, human_member_count, human_member_count_at, online_count, online_count_at)
VALUES ('100000000000000001', 123, '2026-08-01T09:00:00.000Z', 13, '2026-08-01T09:01:00.000Z');
INSERT INTO member_exclusions (guild_id, member_id, reason, updated_at)
VALUES ('100000000000000001', '100000000000000004', 'raid', '2026-08-01T10:00:00.000Z');
INSERT INTO presence_probe
    (guild_id, observed_at, approximate_presence_count, bot_floor, bot_floor_scan_truncated)
VALUES ('100000000000000001', '2026-08-01T10:00:00.000Z', 31, 3, TRUE),
       ('100000000000000001', '2026-08-01T10:01:00.000Z', 30, NULL, FALSE);

INSERT INTO community_stream_heartbeats (guild_id, stream, covered_from, covered_through, updated_at)
VALUES ('100000000000000001', 'event_attended', '2026-07-27T00:00:00.000Z',
        '2026-08-03T00:00:00.000Z', '2026-08-03T00:00:01.000Z');
INSERT INTO community_scorecard_runs
    (id, guild_id, week_start, week_end, classifier_version, watermark, input_count, input_hash,
     idempotency_key, revision, run_status, coverage_state, evidence_state, scorecard_json,
     intervention_code, generated_at)
VALUES (71, '100000000000000001', '2026-07-27T00:00:00.000Z', '2026-08-03T00:00:00.000Z',
        'classifier-v1', 61, 7, 'backup-scorecard-input-hash', 'backup:scorecard:run', 3,
        'completed', 'complete', 'sufficient', '{"score":17,"note":"雪"}',
        'maintain', '2026-08-03T00:00:01.000Z');
INSERT INTO community_scorecard_alerts (guild_id, week_start, alert_key, created_at)
VALUES ('100000000000000001', '2026-07-27T00:00:00.000Z', 'backup:alert:dedupe', '2026-08-03T00:00:02.000Z');
-- Preserve both a spent-but-pending retry budget and terminal completion.
INSERT INTO community_scorecard_attempts
    (guild_id, week_key, attempts, next_attempt_at, completed)
VALUES ('100000000000000001', '2026-08-10', 2, 1786343100000, FALSE),
       ('100000000000000001', '2026-08-03', 1, 1785738000000, TRUE);
INSERT INTO gateway_sessions (guild_id, shard_id, session_id, seq, resume_url, updated_at)
VALUES ('100000000000000001', 2, 'backup-resume-session', 9007199254740993,
        'wss://gateway.example.invalid', '2026-08-01T10:00:00.123456Z');
-- One consumed and one armed one-shot IDENTIFY directive, restored with the
-- gateway checkpoints they govern.
INSERT INTO gateway_boot_directives (guild_id, shard_id, armed_at, reason, consumed_at)
VALUES ('100000000000000001', 0, '2026-08-01T09:00:00Z', 'backup consumed | NULL',
        '2026-08-01T09:05:00.123456Z'),
       ('100000000000000001', 1, '2026-08-01T10:00:00Z', E'backup armed\n雪', NULL);

INSERT INTO guild_settings (guild_id, key, value, version, updated_at, updated_by)
VALUES ('100000000000000001', 'TWO_STICKY_ENABLED',
        '{"enabled":false,"channels":["100000000000000003"],"note":"雪 | NULL","optional":null}',
        83, '2026-08-01T10:00:00.123456Z', '100000000000000002'),
       ('100000000000000001', 'TWO_FEEDS_ENABLED', 'true', 41,
        '2026-08-01T09:00:00Z', '100000000000000004');
-- Must survive exactly, not be bumped by restore's INSERT/TRUNCATE statements.
UPDATE guild_settings_revision SET revision = 211 WHERE singleton = TRUE;
INSERT INTO guild_settings_audit (id, guild_id, key, old_value, new_value, actor, at)
VALUES (79, '100000000000000001', 'TWO_STICKY_ENABLED', NULL,
        '{"enabled":false,"channels":["100000000000000003"],"note":"雪 | NULL","optional":null}',
        '100000000000000002', '2026-08-01T10:00:00.123456Z'),
       (91, '100000000000000001', 'TWO_FEEDS_ENABLED', 'false', NULL,
        '100000000000000004', '2026-08-01T10:01:00Z');
INSERT INTO audit_kill_switch (id, engaged_at, engaged_by)
VALUES (1, '2026-08-01T10:00:00Z', '100000000000000002');

INSERT INTO internal_nonces (nonce_hash, burned_at, expires_at)
VALUES (repeat('a', 64), '2026-08-01T10:00:00Z', '2026-08-01T10:04:01Z');
INSERT INTO internal_clock_high_water (domain, high_water_ms, observed_at)
VALUES ('internal_nonce_db', 1785578400000, '2026-08-01T10:00:00Z');
INSERT INTO internal_idempotency
    (intent_id, caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id,
     target_id, created_at, updated_at, response_code, http_status, resource_id, affected)
VALUES
    (83, repeat('b', 64), repeat('c', 64), 'role.assign', repeat('d', 64), 'completed',
     '100000000000000001', '100000000000000002', '100000000000000004',
     '2026-08-01T10:00:00Z', '2026-08-01T10:00:01Z', 'success', 200, '100000000000000010', 1),
    (89, repeat('b', 64), repeat('e', 64), 'announcement.post', repeat('f', 64), 'unknown',
     '100000000000000001', '100000000000000002', '100000000000000003',
     '2026-08-01T11:00:00Z', '2026-08-01T11:00:01Z', NULL, NULL, NULL, NULL),
    (97, repeat('b', 64), repeat('1', 64), 'event.read', repeat('2', 64), 'in_flight',
     NULL, NULL, NULL, '2026-08-01T12:00:00Z', '2026-08-01T12:00:01Z', NULL, NULL, NULL, NULL);
INSERT INTO internal_action_log
    (audit_id, intent_id, phase, caller_hash, action, guild_id, actor_id, target_id,
     response_code, http_status, evidence_code, created_at)
VALUES
    (101, 83, 'terminal', repeat('b', 64), 'role.assign', '100000000000000001',
     '100000000000000002', '100000000000000004', 'success', 200, 'discord_confirmed_effect', '2026-08-01T10:00:01Z'),
    (103, 89, 'unknown', repeat('b', 64), 'announcement.post', '100000000000000001',
     '100000000000000002', '100000000000000003', NULL, NULL, NULL, '2026-08-01T11:00:01Z'),
    (109, 97, 'intent', repeat('b', 64), 'event.read', NULL, NULL, NULL,
     NULL, NULL, NULL, '2026-08-01T12:00:00Z');
UPDATE internal_idempotency SET resolved_role_id = '100000000000000010' WHERE intent_id = 83;
UPDATE internal_action_log SET resolved_role_id = '100000000000000010' WHERE intent_id = 83;
INSERT INTO internal_discord_events (event_hash, claimed_at)
VALUES (repeat('3', 64), '2026-08-01T10:00:00.123456Z');
INSERT INTO internal_event_keys (guild_id, event_key, event_id, created_at, updated_at)
VALUES ('100000000000000001', 'backup:launch', '100000000000000002',
        '2026-08-01T10:00:00.123456Z', '2026-08-01T10:00:00.123456Z');

INSERT INTO self_role_audit
    (event_id, guild_id, panel_id, member_id, source_id, option_key, role_id, source,
     operation, outcome, code, reason, added_role_ids, removed_role_ids, created_at,
     attempted_added_role_ids, attempted_removed_role_ids, compensated_added_role_ids,
     compensated_removed_role_ids, unresolved_added_role_ids, unresolved_removed_role_ids,
     desired_role_ids, pre_mutation_role_ids, claim_token, claim_generation,
     processing_expires_at, event_order)
VALUES ('backup:self-role:event', '100000000000000001', 'backup:panel', '100000000000000002',
        '100000000000000006', 'backup-option', '100000000000000010', 'component', 'select',
        'processing', 'backup-code', 'backup recovery', '["100000000000000010"]',
        '["100000000000000012"]', '2026-08-01T10:00:00.000Z', '["100000000000000010"]',
        '["100000000000000012"]', '["100000000000000013"]', '["100000000000000014"]',
        '["100000000000000015"]', '["100000000000000016"]', '["100000000000000010"]',
        '["100000000000000012"]', 'backup-self-role-claim', 23,
        '2026-08-01T10:01:00.123456Z', 'backup-event-order');
INSERT INTO self_role_panel_claims
    (guild_id, member_id, panel_id, claim_token, claim_generation, processing_expires_at,
     latest_event_id, latest_option_key, latest_event_order, target_committed)
VALUES ('100000000000000001', '100000000000000002', 'backup:panel', 'backup-self-role-claim',
        23, '2026-08-01T10:01:00.123456Z', 'backup:self-role:event', 'backup-option',
        'backup-event-order', TRUE);
-- Send receipts and uncertainty baselines hang off the seeded audit event, so
-- the complete-schema coverage test archives and restores them too.
INSERT INTO self_role_exchanges
    (exchange_id, event_id, origin_generation, role_id, adding, compensating,
     disposition, created_at)
VALUES ('backup:self-role:exchange', 'backup:self-role:event', 23,
        '100000000000000010', TRUE, FALSE, 'pending', '2026-08-01T10:00:30.000Z');
INSERT INTO self_role_exchange_baselines
    (event_id, legacy_pending, unresolved_added_role_ids, unresolved_removed_role_ids)
VALUES ('backup:self-role:event', FALSE, '[]', '[]');
-- Main's newer durable delivery table rides the same complete-schema coverage:
-- a delivery arbitration claim. gateway_boot_directives is already seeded by
-- main's own consumed+armed rows above; a second (guild, shard 0) row would
-- collide on the primary key.
INSERT INTO automod_delivery_claims
    (guild_id, message_id, delivery_kind, dry_run, request_hash)
VALUES ('100000000000000001', 'backup:message', 'create', FALSE, 'backup-request');

-- Operator erasure receipts carry no subject; accountability survives restore.
INSERT INTO member_erasure_audit (actor, erased_at)
VALUES ('backup-operator', '2026-08-01T10:00:00.123456Z'),
       ('backup-operator', '2026-08-02T10:00:00Z');

-- Tracked short links behind go.two.gg; disabled rows still redirect.
INSERT INTO invite_campaigns (slug, invite_code, label, disabled_at, created_at)
VALUES ('backup-link', 'backup-code', 'backup sidebar', NULL, '2026-08-01T10:00:00.123456Z'),
       ('backup-retired', 'backup-retired-code', 'retired placement',
        '2026-08-02T10:00:00Z', '2026-08-01T10:00:00Z');

-- Temporary voice rooms: one inheriting and one channel-sourced creator; rooms
-- outlive their creator config, so one room points at an unmarked creator.
INSERT INTO voice_creators (guild_id, channel_id, name_template, permission_source,
  permission_channel_id, default_limit, private_default, text_channels,
  text_channel_name, text_viewer_role_id, position, first_room_number)
VALUES ('100000000000000001', '100000000000000030', '{user}''s room', 'creator',
        NULL, NULL, FALSE, FALSE, NULL, NULL, 'above', 1),
       ('100000000000000001', '100000000000000031', 'Squad #{n}', 'channel',
        '100000000000000032', 5, TRUE, TRUE, 'Squad chat', '100000000000000001', 'below', 3);
INSERT INTO voice_rooms (guild_id, channel_id, creator_channel_id, owner_id,
  original_creator_id, name_seed, created_at)
VALUES ('100000000000000001', '100000000000000033', '100000000000000030',
        '100000000000000002', '100000000000000002', '18446744073709551615',
        '2026-08-01T10:00:00.123456Z'),
       ('100000000000000001', '100000000000000034', '100000000000000035',
        '100000000000000003', '100000000000000002', '7', '2026-08-02T10:00:00Z');
-- Accepted create reservations (0417): one bound to the room above, one
-- rolled back (no channel), both settled; plus one still in flight.
INSERT INTO voice_create_reservations (guild_id, user_id, created_at, channel_id, settled_at)
VALUES ('100000000000000001', '100000000000000002', '2026-08-01T09:59:58Z',
        '100000000000000033', '2026-08-01T10:00:00Z'),
       ('100000000000000001', '100000000000000003', '2026-08-02T09:59:58Z',
        NULL, '2026-08-02T10:00:00Z'),
       ('100000000000000001', '100000000000000002', '2026-08-03T10:00:00Z',
        NULL, NULL);
-- V3 privacy (0416): the second room is private with a Join channel and one
-- blocked member; the first stays public with an empty block list.
UPDATE voice_rooms
SET private = TRUE, join_channel_id = '100000000000000038',
    privacy_touched_at = '2026-08-03T10:00:00Z'
WHERE guild_id = '100000000000000001' AND channel_id = '100000000000000034';
INSERT INTO voice_room_blocks (guild_id, room_channel_id, blocked_member_id, created_at)
VALUES ('100000000000000001', '100000000000000034', '100000000000000004',
        '2026-08-03T10:05:00Z');
-- Pending and completed owner-grant provenance must survive backup/restore.
INSERT INTO voice_owner_grants (guild_id, channel_id, member_id, revision, pending, touched_at)
VALUES ('100000000000000001', '100000000000000033', '100000000000000002', 'pending-room-revision', TRUE, '2026-08-01T10:00:00Z'),
       ('100000000000000001', '100000000000000033', '100000000000000003', 'pending-room-revision', TRUE, '2026-08-01T10:00:00Z'),
       ('100000000000000001', '100000000000000034', '100000000000000003', 'completed-room-revision', FALSE, '2026-08-02T10:00:00Z');
-- Companion text channels carry the creation-time settings snapshot; one
-- default-named, one custom-named with an @everyone viewer role.
INSERT INTO voice_text_companions (guild_id, room_channel_id, text_channel_id,
  text_channels, text_channel_name, text_viewer_role_id, created_at)
VALUES ('100000000000000001', '100000000000000033', '100000000000000036',
        TRUE, NULL, NULL, '2026-08-01T10:00:00.123456Z'),
       ('100000000000000001', '100000000000000034', '100000000000000037',
        TRUE, 'Squad chat', '100000000000000001', '2026-08-02T10:00:00Z');
-- Guild room-command controls (0227, on main): one configured guild (creation
-- off, required role, a restricted command and a fail-closed empty list), one
-- defaulted.
INSERT INTO voice_access_controls (guild_id, room_creation_enabled, required_role_id,
  command_roles)
VALUES ('100000000000000001', FALSE, '100000000000000040',
        '{"kick": ["100000000000000041", "100000000000000042"], "template": []}'::jsonb),
       ('100000000000000002', TRUE, NULL, '{}'::jsonb);
-- Guild room logging: one configured guild (full detail, channel, mention
-- role), one at the stored defaults.
INSERT INTO voice_logging_settings (guild_id, detail_level, log_channel_id,
  mention_role_id)
VALUES ('100000000000000001', 'full', '100000000000000050', '100000000000000051'),
       ('100000000000000002', 'brief', NULL, NULL);

-- V11b configuration (0229): one row in every section, including a command
-- restriction with roles and one without, so every table round-trips.
INSERT INTO voice_channel_templates (guild_id, channel_id, name_template, status_template)
VALUES ('100000000000000001', '100000000000000040', 'Lounge {n}', NULL),
       ('100000000000000001', '100000000000000041', 'Stage', 'LIVE');
INSERT INTO voice_game_aliases (guild_id, game, alias)
VALUES ('100000000000000001', 'Some Game', 'SG');
INSERT INTO voice_random_lists (guild_id, name) VALUES ('100000000000000001', 'rooms');
INSERT INTO voice_random_list_choices (guild_id, list_name, position, choice)
VALUES ('100000000000000001', 'rooms', 0, 'den'),
       ('100000000000000001', 'rooms', 1, 'crew');
INSERT INTO voice_logging (guild_id, channel_id, detail)
VALUES ('100000000000000001', '100000000000000042', 'lifecycle');
INSERT INTO voice_logging_mention_members (guild_id, member_id)
VALUES ('100000000000000001', '100000000000000002');
INSERT INTO voice_logging_mention_roles (guild_id, role_id)
VALUES ('100000000000000001', '100000000000000043');
INSERT INTO voice_guild_settings (guild_id, creation_enabled, unique_names, no_game_label,
  force_single_game, count_members_without_activity, time_zone, text_channel_name,
  text_viewer_role_id, command_role_id)
VALUES ('100000000000000001', TRUE, TRUE, 'General', FALSE, TRUE, 'Europe/London',
        'voice-chat', NULL, '100000000000000043');
INSERT INTO voice_command_roles (guild_id, command)
VALUES ('100000000000000001', 'kick'), ('100000000000000001', 'limit');
INSERT INTO voice_command_role_members (guild_id, command, role_id)
VALUES ('100000000000000001', 'kick', '100000000000000043');
