# Cutover migration ledger (offline)

Baseline: `origin/main` at `96ff8512`, read 2026-10-04. No staging, no live
database, no secrets were touched to produce this file.

"Pending" means every migration file present in the repo at that revision and
not yet applied to the production cutover target. The cutover has not run, so
the ledger lists all 70 files across both chains: 8 in
`crates/store/migrations/` (applied by the bot runtime via
`crates/store/src/migrations.rs`) and 62 in `crates/cutover/migrations/`
(applied by the cutover tooling via `crates/cutover/src/staging_migrate.rs`).

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

## Notes

- Voice rows (0224-0229, 0412, 0414, 0416-0418, 0422) are listed here for completeness; their runtime
  rollback disposition belongs to the voice rollback card, not this ledger.
- Unguarded DDL (`CREATE TABLE` / `ADD COLUMN` without `IF NOT EXISTS`) is
  classed `backout-script` even when the change is additive, because a
  re-apply aborts instead of no-opping.
- Per-table post-cutover row measurement is covered by `docs/rollback-delta.md`
  and `crates/cutover/src/rollback_delta.rs`, not by this ledger.
