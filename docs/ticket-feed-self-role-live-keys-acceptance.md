# Ticket, feed and self-role live-keys acceptance

`crates/core/tests/ticket_feed_self_role_live_keys_acceptance.rs` (11 tests)
plus `crates/bot/src/settings_trio_live_log_acceptance_tests.rs` (1 test)
pin the live promotion of `TWO_TICKET_COOLDOWN_SECONDS`,
`TWO_ANNOUNCEMENTS` and `TWO_SELF_ROLE_PANELS`: Hot classification,
`HOT_WIRED` membership, store-first live reads with boot fallback, typed
write refusals, per-key `setting_changed` lines, and untouched B5/B7
boundaries. Both suites are pure and offline: in-memory `live_channel`
only, no database, no network, no process-environment reads.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test ticket_feed_self_role_live_keys_acceptance
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot settings_jobs::trio_live_log_acceptance_tests
```

CI runs them with the other integration targets (`cargo test --workspace --test '*'`).

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | The trio classifies `Hot`, joins `HOT_WIRED`, and is storable | `trio_keys_are_classified_hot_and_hot_wired` |
| 2 | Live poll reports the trio hot (never cold) with env-string from/to, serves per guild with no cross-guild leak, and the panels rendering round-trips through the catalogue loader | `trio_rows_report_hot_with_env_renderings_on_live_poll` |
| 3 | Stored rows override boot env store-first through the real loaders in both directions (cooldown value, announcements gate, parsed catalogue) | `trio_stored_rows_override_boot_env_store_first` |
| 4 | Deletes report hot changes back to boot (`new` = `None`) and the layered map carries boot again | `trio_deletes_report_hot_changes_back_to_boot` |
| 5 | Invalid writes are refused before any SQL with typed codes: the announcements gate takes only boolean/0/1 (`InvalidGateValue`); cooldown and panels refusals name the key in both the code and the next-action message; valid values and deletes validate as `Hot` | `announcements_invalid_writes_refused_with_typed_gate_code`, `cooldown_invalid_writes_refused_naming_key`, `panels_invalid_writes_refused_naming_key` |
| 6 | B5 exclusion keys stay `EnvOnly`, refuse writes, and are ignored on refresh; B7 gaps (escalation, mute role) stay uninvented | `b5_exclusion_keys_stay_env_only_and_refuse_writes`, `b7_escalation_and_mute_role_gaps_stay_uninvented` |
| 7 | Fallback premises stay safe: absent rows read unset, the catalogue loader refuses malformed input without panicking, only exactly `"1"` enables the gate, numerics render as plain integers | `fallback_premises_absent_rows_and_loader_refusals_stay_safe` |
| 8 | Already-wired keys stay hot-wired (control) | `control_feed_interval_and_raid_threshold_stay_hot_wired` |
| 9 | A live poll carrying trio rows emits exactly one `setting_changed` line per key inside a `settings_applied` swap, never `settings_restart_required` | `trio_live_poll_emits_setting_changed_per_key` |

## Consumer wiring

Each key has a live reader plus live-apply pins in its consumer's own test
file (store-first over boot, last-good/boot fallback on absent or
unparsable rows):

- Cooldown: the ticket runtime refresh resolves it from the layered
  snapshot (`ticket_runtime.rs`), so a stored value reaches ticket
  operations on the next per-operation refresh.
- Announcements: the feed poller re-checks the gate on every tick and
  skips while a stored `0` stands (`feed_jobs.rs`).
- Panels: self-role dispatch and recovery resolve the catalogue on every
  interaction and sweep (`self_role_handlers.rs`). Catalogue changes and
  deletes refuse until durable processing, terminal repair and unknown
  exchanges drain. Admission takes a shared settings-revision lock and
  rechecks the pinned snapshot revision, so a stale input cannot create work
  after a catalogue edit commits. Unknown sends may deliberately block edits
  indefinitely; restoring or bypassing them with SQL is not a safe undo.
  Boot catalogue changes also require draining before restart. Panels are
  excluded from the independently expiring ops overlay; use the settings API.

Builder regression controls supplement the protected acceptance suites:

- `settings_consumer_wiring_tests.rs` installs the real global reader in isolated
  subprocesses and calls actual component, stale-control, reaction, admission,
  recovery and fenced feed-registration entry points. Feed action counts follow
  stored `1 → 0 → 1` ticks, then deletion back to boot.
- `ticket_runtime_tests.rs` pins ops cooldown apply, clear and expiry at the same
  database revision. Refresh compares effective values, not just that revision.
- `self_role_catalogue_drain.rs` exercises catalogue edit/delete refusal,
  processing and terminal/pending work, the admission/writer lock, and stale
  admission after retirement in a disposable test database.
- `self_role_ops_boundary.rs` and `ops-self-role-boundary.test.ts` pin exclusion
  from ops ingest/read and PUT respectively.

Service existence stays boot-gated by design: a boot-off feed poller
registers no job and a boot-empty catalogue builds no self-role service,
so enabling from a boot-disabled deployment needs a restart (same reason
`TWO_AUTOMOD` stays cold). Registration, the live-identity fence, boot
validation, and the announcement command surface are unchanged.

## Not covered here

- The staging live apply-check stays a rollout step (this slice pins the
  mechanism offline; it performs no live apply).
- Dashboard pages, voice verbs, `/ops` enforcement, new keys, and
  production enablement are out of scope.
- Per-key refusal codes map to `Malformed`/`settings_value_invalid` at the
  internal-action boundary (`crates/cutover/src/internal_settings.rs`).
