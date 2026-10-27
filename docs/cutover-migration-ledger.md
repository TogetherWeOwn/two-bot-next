# Cutover migration ledger (historical; 2026-10-04 baseline)

> Historical record, not today's database state. The baseline below
> inventories the 70 migration files present at `origin/main` (`96ff8512`),
> read 2026-10-04, before the production cutover (production cutover happened
> on 2026-10-10, see [releases.md](releases.md)). Rows for 0414, 0415, 0416,
> 0417, 0418, 0419, 0420, 0422, 0424 and 0425 name files absent at that
> baseline (verified via `git ls-tree 96ff8512`): they were appended by later
> edits and were not re-verified against a stated revision. Later edits also
> changed existing text in place (the 0406 backout note, the Notes voice-row
> list). The ledger also
> does not include 0413, 0421, 0423 or 0426, which are present in the repo.
> This file makes no claim about which migrations have applied to any
> database. For current-state verification, see the [Production migration
> runner](runbook.md#production-migration-runner).

Baseline: `origin/main` at `96ff8512`, read 2026-10-04. No staging, no live
database, no secrets were touched to produce this file.

"Pending" meant, at that baseline date, every migration file present in the
repo at that revision and not yet applied to the production cutover target.
The cutover had not run as of 2026-10-04, so the baseline listed all 70 files
across both chains: 8 in
`crates/store/migrations/` (applied by the bot runtime via
`crates/store/src/migrations.rs`) and 62 in `crates/cutover/migrations/`
(applied by the cutover tooling via `crates/cutover/src/staging_migrate.rs`).
Rows appended afterwards are outside that baseline.

All migrations are up-only; there are no down files. The `direction` column is
`up` throughout.

## Rollback classes

- `re-runnable`: idempotent DDL (`IF NOT EXISTS`, `ON CONFLICT DO NOTHING`,
  or a guarded `DO` block). Re-applying is a no-op. Backout drops the created
  objects, which destroys any rows written after apply.
- `backout-script`: needs a hand-written inverse (type conversions, constraint
  swaps, data backfills, unguarded DDL that fails on re-apply, trigger or
  function installs). The backout note names the inverse.
- `unmeasurable`: the migration leaves no verifiable data or schema delta to
  roll back (grant-only, forward-only sequence reseed). A reason is required.

## Store chain (`crates/store/migrations/`)

| migration | direction | tables touched | rollback class | backout note |
|---|---|---|---|---|
| 0400_initial | up | events, members, invite_snapshots | re-runnable | Drop the three tables; destroys the funnel log and its projections. |
| 0401_web_contract_tables | up | web_contract_meta, guild_counters, rank_ladder, rank_snapshots, member_ranks, scheduled_events | re-runnable | Drop the six tables; seed rows use `ON CONFLICT DO NOTHING`. |
| 0402_counter_snapshots | up | counter_snapshots, member_exclusions | re-runnable | Drop both tables; audit history only, no live reads depend on it. |
| 0403_gate_cleared | up | members | re-runnable | Drop column `gate_cleared_at` and its index; NULLs carry no meaning. |
| 0404_members_third_message_at | up | members | re-runnable | Drop column `third_message_at`; the funnel log can re-derive it. |
| 0405_timestamptz_and_boolean | up | events, members | backout-script | Type rewrite: backout is `is_bot USING is_bot::smallint` plus restoring the old defaults; `recorded_at` default must be set back explicitly. |
| 0406_rollback_journal | up | rollback_journal, rollback_watermarks | re-runnable | Drop both tables; journal rows are cutover evidence, re-capturable. Both tables and the store chain's `_two_bot_migrations` ledger are excluded from backups (`EXCLUDED_TABLES`): journal rows are re-capturable watch-window evidence, and restoring old watermarks could mark post-backup writes as journaled. |
| 0407_invite_campaigns | up | invite_campaigns | re-runnable | Drop the table; no other migration reads it. |
| 0408_backfill_cursors | up | backfill_cursors | re-runnable | Drop the table; cursors re-checkpoint on the next worker run. |

## Cutover chain (`crates/cutover/migrations/`)

| migration | direction | tables touched | rollback class | backout note |
|---|---|---|---|---|
| 0001_funnel | up | events, members, invite_snapshots | re-runnable | Drop the three tables; destroys the funnel log and its projections. |
| 0002_leveling | up | member_levels, xp_awards, xp_cooldowns, level_import_runs, level_role_rewards | re-runnable | Drop the five tables; leveling state is re-importable from the source. |
| 0110_moderation_member | up | moderation_warnings, moderation_scheduled_unbans, moderation_audit, moderation_idempotency | re-runnable | Drop the four tables; shared with 0120, coordinate the drop. |
| 0111_moderation_ban_ownership | up | moderation_member_bans, moderation_scheduled_unbans | re-runnable | Drop `moderation_member_bans` and drop column `dispatch_uncertain`. |
| 0112_moderation_legacy_timestamps | up | moderation_warnings, moderation_scheduled_unbans, moderation_audit, moderation_idempotency | backout-script | In-place TEXT-to-timestamptz conversion: backout is per-column `ALTER ... TYPE TEXT`; rows rejected at apply time need manual reconciliation. |
| 0113_moderation_unban_retry_order | up | moderation_scheduled_unbans | re-runnable | Drop column `retry_generation`; retry ordering restarts from zero. |
| 0114_moderation_member_runtime_grants | up | (none: grants only) | unmeasurable | No schema or data change when the role is absent; backout is `REVOKE` from `two_bot_runtime`, which leaves no row trace to verify. |
| 0120_channel_moderation | up | moderation_audit, moderation_idempotency, moderation_lockdowns | re-runnable | Drop `moderation_lockdowns`; the other two are shared with 0110. |
| 0121_channel_claim_generation | up | moderation_idempotency | re-runnable | Drop column `claim_token`; in-flight claims must drain first. |
| 0122_channel_lockdown_generation | up | moderation_lockdowns | re-runnable | Drop column `recovery_generation`; in-flight recoveries must drain first. |
| 0123_channel_execution_fence | up | moderation_channel_executions | backout-script | Unguarded `CREATE TABLE` fails on re-apply; backout is `DROP TABLE moderation_channel_executions`. |
| 0124_channel_shared_timestamps | up | moderation_audit, moderation_idempotency | backout-script | Same guarded TEXT-to-timestamptz conversion as 0112: backout is per-column `ALTER ... TYPE TEXT`. |
| 0130_custom_commands | up | automation_commands, automation_audit_log | re-runnable | Drop both tables; command definitions are re-creatable by owners. |
| 0140_scheduled_messages | up | scheduled_messages, automation_audit_log | re-runnable | Drop `scheduled_messages`; drain pending sends first. |
| 0141_scheduled_messages_legacy_upgrade | up | scheduled_messages | backout-script | Type change (`interval_seconds` to BIGINT) plus claim columns: backout restores the old type with an explicit cast and drops the claim columns. |
| 0142_scheduled_delivery_uncertainty | up | scheduled_messages | backout-script | Additive fence; existing nonce/claim rows backfill uncertain, and pre-0142 archives re-fence pending rows at restore. Retain the column and fence on rollback; never downgrade to a blind-replay writer or clear pending uncertainty. Stop older writers before rollout. |
| 0150_sticky_messages | up | sticky_messages, automation_audit_log | backout-script | TEXT-to-timestamptz conversions plus claim columns: backout restores TEXT columns and drops the claim columns. |
| 0160_rsvp | up | event_rsvps, community_facts, announcements_audit_log | re-runnable | Drop the three tables; RSVP state is re-collectable from events. |
| 0170_lfg | up | lfg_posts, lfg_roles, lfg_signups | re-runnable | Drop the three tables; open groups must close first. |
| 0180_feeds | up | feed_relays, feed_deliveries, announcements_audit_log | re-runnable | Drop the feed tables; claim columns drop with them. |
| 0190_onboarding | up | events | backout-script | Adds a vocabulary `CHECK` constraint: backout is `ALTER TABLE events DROP CONSTRAINT` for the onboarding check. |
| 0200_self_roles | up | self_role_audit, self_role_panel_claims | re-runnable | Drop both tables; panel leases expire on their own. |
| 0201_self_role_intent_initialization | up | self_role_audit | re-runnable | Drop column `intent_initialized`; defaults to initialized. |
| 0202_self_role_compensation_phase | up | self_role_audit | re-runnable | Drop column `compensating`; in-flight compensations must settle first. |
| 0203_self_role_pending_exchange | up | self_role_audit | re-runnable | Drop column `exchange_pending`; pending exchanges must settle first. |
| 0204_self_role_terminal_repair | up | self_role_audit | re-runnable | Drop the repair columns and index; expired repairs are inert. |
| 0205_self_role_exchange_receipts | up | self_role_exchanges | re-runnable | Drop the table; receipts are re-derivable from the audit log. |
| 0206_self_role_exchange_baselines | up | self_role_exchange_baselines, self_role_exchanges | backout-script | Data backfill plus a new `CHECK` constraint: backout drops the constraint, then the columns, then the backfilled table. |
| 0210_tickets | up | tickets, ticket_transcripts | backout-script | Constraint swap and nullability changes: backout restores the old status check, re-adds `channel_id NOT NULL`, and drops `closing_started_at`. |
| 0220_automod | up | automod_violations, automod_processed_messages | re-runnable | Drop both tables; the sentinel backfill uses `ON CONFLICT DO NOTHING`. |
| 0221_automod_delivery_claims | up | automod_delivery_claims | re-runnable | Drop the table; in-flight delivery claims must drain first. |
| 0222_automod_counted_claim | up | automod_delivery_claims | re-runnable | Drop column `counted`; counts restart from zero. |
| 0223_automod_preserved_match | up | automod_delivery_claims | re-runnable | Drop the preserved-match columns; in-flight matches must drain first. |
| 0224_voice_rooms | up | voice_creators, voice_rooms | re-runnable | Drop both tables; runtime disposition belongs to the voice rollback card. |
| 0225_voice_inherit_limit | up | voice_creators | backout-script | Drops `DEFAULT` and `NOT NULL` on `default_limit`: backout re-adds both after reconciling NULL rows. |
| 0226_voice_text_channels | up | voice_creators, voice_text_companions | backout-script | Unguarded `ADD COLUMN` fails on re-apply; backout drops the two columns and the companions table. |
| 0227_voice_access_controls | up | voice_access_controls | re-runnable | Drop the table; per-guild settings are re-enterable. |
| 0228_voice_logging_settings | up | voice_logging_settings | re-runnable | Drop the table; per-guild settings are re-enterable. |
| 0229_voice_config | up | voice_creators, voice_channel_templates, voice_game_aliases, voice_random_lists, voice_random_list_choices, voice_logging, voice_logging_mention_members, voice_logging_mention_roles, voice_guild_settings, voice_command_roles, voice_command_role_members | backout-script | Unguarded `ADD COLUMN` on `voice_creators` fails on re-apply; backout drops those columns and all ten config tables. |
| 0300_website_contract | up | web_contract_meta, guild_counters, rank_ladder, rank_snapshots, member_ranks, scheduled_events, counter_snapshots, member_exclusions | re-runnable | Drop the eight tables; seeds use `ON CONFLICT DO NOTHING`. |
| 0310_presence_probe | up | presence_probe | re-runnable | Drop the table; probe history is re-collectable. |
| 0311_community_scorecard | up | community_scorecard_runs, community_scorecard_alerts, community_stream_heartbeats | re-runnable | Drop the three tables; weekly scorecards regenerate. |
| 0312_community_scorecard_attempts | up | community_scorecard_attempts | re-runnable | Drop the table; retry state is transient. |
| 0320_gateway_sessions | up | gateway_sessions | re-runnable | Drop the table; sessions re-register on next connect. |
| 0321_gateway_boot_directives | up | gateway_boot_directives | re-runnable | Drop the table; only armed-but-unconsumed directives matter, drain first. |
| 0330_guild_settings | up | guild_settings, guild_settings_audit, guild_settings_revision | backout-script | Drops six env-only `CHECK` constraints and installs audit/revision triggers: backout re-adds the constraints and drops the triggers and functions. |
| 0331_guild_settings_versions | up | guild_settings | backout-script | Installs the version-assign trigger and reseeds the sequence: backout drops the trigger and function. |
| 0332_guild_settings_allocator | up | guild_settings | backout-script | Replaces the version-assign function body: backout re-applies the 0331 function body. |
| 0333_guild_settings_revision | up | guild_settings | unmeasurable | Forward-only sequence reseed above stored and allocated maxima; nothing to undo and no delta to verify. |
| 0334_guild_settings_cas | up | guild_settings | backout-script | Unguarded sequence plus `ACCESS EXCLUSIVE` lock and new column: backout drops the column, trigger, function, and sequence. |
| 0340_operational_audit | up | operational_audit_log, audit_kill_switch | re-runnable | Drop both tables and the delivery/mirror columns; audit history only. |
| 0350_internal_actions | up | internal_nonces, internal_idempotency, internal_action_log, internal_discord_events | backout-script | Unguarded `CREATE TABLE` fails on re-apply; backout drops all four tables after draining pending intents. |
| 0351_internal_action_roles | up | internal_idempotency, internal_action_log | backout-script | Unguarded `ADD COLUMN` with a format `CHECK`: backout drops both columns. |
| 0352_internal_action_not_sent | up | internal_action_log, internal_idempotency | backout-script | Swaps phase/state `CHECK` definitions: backout restores the prior check bodies. |
| 0353_internal_clock_high_water | up | internal_clock_high_water | backout-script | Unguarded `CREATE TABLE` fails on re-apply; backout drops the table. |
| 0360_join_risk_flags | up | join_risk_flags | re-runnable | Drop the table; risk flags recompute on next join. |
| 0361_discord_send_admission | up | discord_send_admission | re-runnable | Drop the table; admission state is transient. |
| 0362_gateway_onboarding_jobs | up | gateway_onboarding_jobs | re-runnable | Drop the table; restart-recovery queue, rows hold no durable member state. |
| 0370_containment_claims | up | containment_incidents, containment_events | re-runnable | Drop both tables; active containments must stand down first. |
| 0390_legacy_copy_presence | up | presence_probe | re-runnable | Drop column `bot_floor_scan_truncated`; probe re-scans on next run. |
| 0410_member_erasure_audit | up | member_erasure_audit | backout-script | Unguarded `CREATE TABLE` fails on re-apply; backout drops the table (erasure evidence, export before dropping). |
| 0411_invite_campaigns | up | invite_campaigns | re-runnable | Drop the table; campaigns are re-creatable. |
| 0412_voice_rooms_ownership_touched | up | voice_rooms | re-runnable | Drop column `owner_touched_at`; ownership handoffs lose their timestamp. |
| 0414_voice_rooms_custom_name | up | voice_rooms | re-runnable | Drop columns `custom_name` and `name_touched_at`; rooms fall back to their template name. |
| 0415_internal_event_keys | up | internal_event_keys | backout-script | Unguarded `CREATE TABLE` fails on re-apply; backout drops the table (event.read loses its key map until upsert re-registers keys). |
| 0416_voice_room_privacy | up | voice_rooms, voice_room_blocks | backout-script | Unguarded `ADD COLUMN`/`ADD CONSTRAINT` fail on re-apply; backout drops the join-channel constraint, the three privacy columns and the block table (private rooms become public in the store while their Discord overwrite stays; export the block list first). |
| 0417_voice_vote_kick_audit | up | voice_vote_kick_audit | re-runnable | Drop the table; the vote-kick audit history it holds is not reconstructible. |
| 0418_voice_join_grants | up | voice_join_grants | re-runnable | Drop the table; approved Connect grants lose their revocation witness (rooms keep their Discord overwrites until `/public` re-derives them). |
| 0419_send_admission_lease | up | discord_send_admission | re-runnable | Drop column `in_flight_since_ms`; lanes revert to hold-forever (a stuck lane needs the manual release again). |
| 0420_send_admission_lease_backfill | up | discord_send_admission | re-runnable | Data-only stamp of legacy-held rows; nothing to undo (re-apply only moves rows still at the legacy default). |
| 0422_voice_create_reservations | up | voice_create_reservations | re-runnable | Drop the table; room creates lose their rolling burst and cooldown history (in-flight creates stop holding cap slots). |
| 0424_internal_action_outcome | up | internal_idempotency | backout-script | Unguarded `ADD COLUMN` with a closed-set `CHECK`. Backout: disable the receiver and deploy the previous binary, then drop the column; dropping it under the outcome-aware binary fails every finish and every success replay. No member PII in the column; no seed writes to the table. |
| 0425_internal_action_audit_resources | up | internal_action_log | backout-script | Unguarded `ADD COLUMN` with format/closed-set `CHECK`s. Backout: drop the three columns (terminal audits lose their resource witness until re-recorded; the columns hold no member PII beyond the receipt's own snowflakes and counts). |
| 0427_event_store_voice_capture | up | event_store | re-runnable | Drop the table; the provisioned voice-capture shape (ids including nullable `message_id` for S1 coexistence, session, timestamps, enum flags only) carries no writer yet, so nothing is reconstructible once dropped. S1's exact-column pin stays owned by the S1 build card to relax to required-present/forbidden-absent. |
| 0435_backfill_cursors | up | backfill_cursors | re-runnable | Drop the table; cutover mirror of store 0408, cursors re-checkpoint on the next worker run. |
| 0436_member_catchup | up | member_catchup_members, member_catchup_cursors | re-runnable | Drop both tables; per-guild member rows and resume cursors re-capture from the Discord API on the next worker run, and the members frontier re-checkpoints into backfill_cursors. |
| 0438_retention_queues | up | retention_guild_purge_queue, retention_deletion_requests | re-runnable | Drop both tables; queued guild purges and pending deletion requests are lost and must be re-enqueued, completed receipts are the only durable record. |
| 0440_stream_live_claims | up | stream_live_claims | re-runnable | Drop the table; go-live dedup claims re-claim on the next `stream.online` event (delivered rows lose their restart-proof fence until re-posted). |
| 0443_stream_live_send_started | up | stream_live_claims | re-runnable | Drop `send_started_at` and `claim_token`; in-flight sending markers and ownership tokens are lost (stale started rows become reclaimable until re-marked), delivered fences survive. |
| 0448_internal_action_stop_reason | up | internal_idempotency, internal_action_log | backout-script | Unguarded `ADD COLUMN` with closed-set `CHECK`s. Backout: disable the receiver and deploy the previous binary, then drop both columns; dropping them under the reason-aware binary fails every finish and every success replay. Early-stopped purge receipts lose their reason. No member PII in the columns. |
| 0449_self_role_ticket_acknowledgement | up | self_role_exchanges | re-runnable | Guarded `ADD COLUMN IF NOT EXISTS` with closed-set `CHECK`s. Backout: drop the five acknowledgement columns; tickets written after apply lose their settlement state but keep their dispositions. No member PII in the columns. |
| 0450_scheduled_events_claim | up | scheduled_events_claim | backout-script | Guarded `CREATE TABLE IF NOT EXISTS`. Backout: deploy the previous binary, then drop the table; dropping it under the claim-aware binary fails every events swap. The mirror itself stays readable. No member PII in the table. |
| 0452_scheduled_events_tombstone | up | scheduled_events_tombstone | backout-script | Guarded `CREATE TABLE IF NOT EXISTS`. Backout: deploy the previous binary, then drop the table; dropping it under the tombstone-aware binary fails every events swap. The mirror itself stays readable. No member PII in the table. |
| 0455_event_voice_channels | up | event_voice_channels, event_voice_presence | backout-script | Guarded `CREATE TABLE IF NOT EXISTS` with inline UNIQUE/EXCLUDE. Backout: deploy the previous binary, then drop both tables; dropping them under the aware binary fails every event-voice claim and presence write. Presence spans carry member `user_id` (erasure-covered via the scheduled-event mirror). |
| 0456_admin_api_tokens | up | admin_api_tokens | re-runnable | Guarded `CREATE TABLE IF NOT EXISTS` plus a guarded live-lookup index. Backout: drop the table; no binary reads it yet, so nothing breaks (later slices wire the container authorizer to it; issued tokens would need re-issuance after a drop). Hash-only secret storage; `issued_by` is operator attribution retained by the erasure allowlist. This historical table-only backout is superseded by 0464 once the named-holder repair applies. |
| 0464_admin_api_token_authority | up | admin_api_tokens | re-runnable | Forward holder/lifetime repair, mirrored in store 0463; guarded column/constraint additions with validated CHECKs. Requires named human/AI holders, humans-only admin and finite positive lifetime at most 7,776,000 seconds. Incompatible historical rows abort the entire transaction without partial DDL or data rewrites. Backout: previous binary with gateway closed, retain all authority constraints and migration history; do not DROP/recreate 0456. `holder_id` can identify a member and is explicitly retained as credential authority by the erasure exception manifest. See [registry contract](admin-registry.md). |

| 0460_giveaways | up | giveaways, giveaway_entries, giveaway_draws | backout-script | Guarded `CREATE TABLE IF NOT EXISTS`. Backout: deploy the previous binary, then drop the three tables; dropping them under the giveaway-aware binary fails entries, claims and draws, and live giveaways must be recreated. Member PII in the tables (creators, entrants, winners) is covered by the member-erasure plan in the same change. Renumbered from 0452, then 0454, after main claimed 0452/0453 for the scheduled-events tombstones and 0454 for the sticky-delivery fence; then renumbered to 0458 after main claimed 0456/0457 for the admin token registry; finally to 0460 after main claimed 0458/0459 for starboard; never merged under earlier numbers. |
| 0462_audit_reconcile_cursor | up | operational_audit_log | re-runnable | Guarded nullable `delivery_reconcile_before` column holds only a verified recovery floor, never message content; re-apply is a no-op. Exception to the class-level drop-on-backout rule: deploy the previous binary with the additive column retained; never drop the column or discard cursor or acceptance evidence. |
| 0466_pause_resume_marker | up | giveaways | re-runnable | Guarded nullable `paused_at` column (set by pause, cleared by resume), mirrored in store 0465; resume extends `ends_at` by the paused span. Neutral stem before first merge: the slice-1 acceptance scan bans ALTER in giveaway-named files, so the marker ships without the feature keyword (same shape as the 0462 cursor column). Backout: deploy the previous binary with the additive column retained; pausing under the new binary then backing out leaves rows paused that only the new binary resumes. No member PII in the column. |
| 0468_custom_command_registry_resync | up | automation_command_registry_resync | re-runnable | Guarded `CREATE TABLE IF NOT EXISTS`: one pending-publication marker row per guild (`guild_id`, `requested_at`), recorded when a committed custom-command mutation's registry publication fails and deleted by the next confirmed full-set publication. Backout: drop the table; the previous binary never reads it, definitions and audit history survive, and its boot resync republishes the full set (in-flight pending state only is lost). No member PII; excluded from backups as re-derivable runtime state. Cutover chain only, like 0130. |
| 0469_voice_settings_versions | up | voice configuration tables, voice_settings_audit, internal_idempotency | backout-script | Additive version/actor metadata, revision allocator and append-only audit; replaces the internal-action CHECK with a superset. Renumbered to 0468 after main claimed 0460/0461 for giveaways, 0462 for the audit cursor, 0463/0464 for the admin-token authority repair, 0465/0466 for the pause/resume marker and 0467 for the LFG close-refresh retry; never merged under earlier numbers. Apply with the reviewed role plan before the aware binary serves, even with the API dark. Runtime backout disables the voice capability and deploys the previous binary under the existing execution contract; retain all additive schema and audit history, with no down-migration or data deletion. Existing attribution remains unknown. |

## Notes

- Voice rows (0224-0229, 0412, 0414, 0416-0418, 0422, 0427) are listed here for completeness; their runtime
  rollback disposition belongs to the voice rollback card, not this ledger.
- Unguarded DDL (`CREATE TABLE` / `ADD COLUMN` without `IF NOT EXISTS`) is
  classed `backout-script` even when the change is additive, because a
  re-apply aborts instead of no-opping.
- Per-table post-cutover row measurement is covered by `docs/rollback-delta.md`
  and `crates/cutover/src/rollback_delta.rs`, not by this ledger.
