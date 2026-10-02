# Legacy Postgres copy

`legacy_copy` is an **offline**, source-read-only, batched copy tool. It makes no
Discord calls, never runs migrations, never deletes data, and is **not cutover
or production authorization**. Building/testing it does not authorize execution
against real databases.

## Operator interface

```sh
# No connection: complete mapping/pending inventory, JSON format version 1.
cargo run -p two-bot-cutover --bin legacy_copy --locked -- --plan

# Source and target URLs are supplied separately. Prefer tool-specific env
# variables to keep credentials out of process arguments/shell history.
# LEGACY_COPY_SOURCE_URL and LEGACY_COPY_TARGET_URL must already be provisioned.
cargo run -p two-bot-cutover --bin legacy_copy --locked -- \
  --groups funnel,leveling,website_contract,presence,community,guild_settings,operational_audit

# Only after separate cutover approval and stopping BOTH bot writers:
# repeat the same command with --apply. Unknown/live/staging targets also need
# --allow-live-target; the flag itself grants no authority.
```

Optional flags: `--source-url URL`, `--target-url URL`, `--batch-size 1..10000`
(default 500), `--groups ready|all|group,...` (default `ready`), `--apply`,
`--allow-live-target`, `--plan`, `--help`. Unknown/duplicate arguments and flag
values such as `--apply=false` refuse with exit 2. Pending/retired group selection
refuses with its name and reason **before any connection**. `all` refuses while
any group remains pending; `ready` explicitly selects the seven compatible groups.
The full inventory is always printed, including pending groups, so it is not a
claim of whole-bot parity.

URLs require explicit user, password (empty allowed), host and database. Explicit
empty passwords are pinned empty rather than falling back to `PGPASSWORD` or a
passfile. There is no fallback to `DATABASE_URL`, `TWO_DATABASE_URL`, another
binding, or another credential after failure. Connection/database diagnostics
withhold raw errors: PostgreSQL errors can contain row values or credentials.
Regular/slow statement logging is disabled. Output contains mappings and counts,
not URL fields or copied row payloads.

The target fence accepts without an override **only**
`postgres[ql]://agent_test:@agent-testdb:5432/two_bot_test_<lowercase-safe-suffix>`
(maximum database name length 63), with no query/fragment, socket or startup-option
override. Everything else is treated as live/unknown—even staging, loopback,
Neon, and a hostname containing `test`. Source and target cannot resolve to the
same host/port/database tuple. This check cannot prove that two different DNS
aliases are different servers; the approved cutover operator must also verify
endpoint identity. No target connection is made before the fence passes.

## Schema provenance and provisioning

The 54 legacy SQL files in `crates/cutover/tests/fixtures/legacy_migrations` are
verbatim fixtures from [TogetherWeOwn/two-bot at
96777468472f23a02a1e97a43ffab3912fe5df2a](https://github.com/TogetherWeOwn/two-bot/tree/96777468472f23a02a1e97a43ffab3912fe5df2a/migrations).
The range is 0001–0042, with repeated numeric prefixes and no fabricated missing
numbers. Tests apply them lexically by full filename; SQLx numeric migration
tracking is not suitable for this legacy fixture set.

The target is the shipped `crates/cutover/migrations` set. At implementation the
main snapshot was `44338b2`; `crates/store/migrations` did not exist. This PR adds
**0390**, an additive preservation of legacy 0041's
`presence_probe.bot_floor_scan_truncated` flag. It does not alter immutable 0310.
Provision target schemas separately, including 0390, before running this tool.
Missing columns/tables/sequence/conflict arbiters produce a named database refusal,
not schema creation or silent column loss. Views, SQLx/schema migration ledgers,
and target-only settings revision state are not legacy application-data tables.
Website views must be provisioned by the website contract path separately.

## Copy semantics

- All mappings are compiled constants with explicit source/target columns,
  destination cast types, native source key tuple and target conflict tuple.
  Tables resolve in `public`; identifiers are quoted, values are bound.
- Source counts/projections use one UTC read-only repeatable-read snapshot.
  All selected source/target plans and sequence relations are preflighted before
  the first target write. Dry-run prints source counts and mapping; it performs
  no target DML, sequence allocation or DDL. CLI dry-run target connections are
  read-only as an additional wall.
- Keyset pagination reconstructs native source keys via a typed record. It does
  not use OFFSET, text-sorted numeric keys or a whole-table in-memory payload.
  Rows such as IDs 2, 10 and 9007199254740993 retain native order/precision.
- A target batch transaction locks that table, compares typed mapped values,
  then executes one atomic conditional upsert only for changed/missing rows.
  Unchanged batches issue **no INSERT**, so settings statement-level revision
  triggers do not advance on replay. Unmapped target columns are never updated.
- `guild_settings_audit` is insert-only. Equal existing rows replay as no-ops;
  divergent existing IDs refuse. Target append-only triggers remain enabled.
- Explicit identity/version sequences advance to the target maximum only when
  behind it, never backwards. Each committed batch is reconciled before commit.
  [PostgreSQL sequence operations are not transactional](https://www.postgresql.org/docs/18/functions-sequence.html):
  rollback may leave a harmless forward gap. Both runtimes and other allocators
  must be stopped. No promise of gapless sequences or online-copy safety.
- Audit delivery rows must already be `none`, `delivered` or `quarantined`.
  Any selected unresolved/pending/delivering/unknown state refuses in source
  preflight. Copying claim tokens into a new delivery implementation must not
  become authorization to replay external effects. Terminal evidence and the
  administrative `audit_kill_switch` are preserved.
- Restart the same selection after interruption. Already committed batches
  replay without DML; a lost acknowledgement cannot duplicate primary keys.
  This is replay-based resumption, **not** an external persisted cursor. A new
  run uses a new snapshot; stop legacy writers for the final copy. No source
  deletion replication, stale-row cleanup or concurrent-next-write arbitration.
- Alternate unique-key collisions fail; they never overwrite an unrelated
  primary-key row. Whole-copy atomicity is not claimed: earlier batches remain
  committed if a later conversion/constraint/write fails. Keep runtimes stopped,
  diagnose the named table, fix provisioning/source data under separate approval,
  then replay. Dry-run preflights types and counts, not every data constraint.

The JSON transport uses [PostgreSQL's documented typed record conversion](https://www.postgresql.org/docs/18/functions-json.html#FUNCTIONS-JSON-PROCESSING-TABLE)
and [SQLx 0.9 explicit connection options](https://docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgConnectOptions.html).
Funnel/leveling/audit/settings timestamps are cast to TIMESTAMPTZ. Legacy 0009
BOOLEAN values remain BOOLEAN. Website/presence/community ISO timestamps and
JSON metadata deliberately remain TEXT. Settings JSONB remains JSONB, including
nested values and SQL NULLs. Discord IDs remain TEXT.

## Mapping table

Each entry below is source table → same-named target table. Within the typed
column list, every `name:type` means `source.name::type → target.name`. The
compiled manifest (`--plan`) includes the same per-column mapping and source
keys/conflict keys, plus sequence and append-only policy. Parent `rank_ladder`
is copied before referencing snapshots/member ranks.

| Group / table | Native key = conflict key | Mapped columns and casts |
|---|---|---|
| funnel / `events` | `id` | `id:bigint`, `event_type:text`, `member_id:text`, `guild_id:text`, `occurred_at:timestamptz`, `recorded_at:timestamptz`, `source:text`, `metadata:text`, `idempotency_key:text` |
| funnel / `members` | `guild_id, member_id` | `guild_id:text`, `member_id:text`, `joined_at:timestamptz`, `join_source:text`, `first_message_at:timestamptz`, `first_voice_at:timestamptz`, `last_active_at:timestamptz`, `left_at:timestamptz`, `inactive_flagged_at:timestamptz`, `is_bot:boolean`, `gate_cleared_at:timestamptz`, `third_message_at:timestamptz` |
| funnel / `invite_snapshots` | `guild_id, code` | `guild_id:text`, `code:text`, `uses:integer`, `inviter_id:text`, `channel_id:text`, `updated_at:timestamptz` |
| leveling / `member_levels` | `guild_id, member_id` | `guild_id:text`, `member_id:text`, `xp:bigint`, `message_xp:bigint`, `voice_xp:bigint`, `imported_xp:bigint`, `updated_at:timestamptz` |
| leveling / `xp_cooldowns` | `guild_id, member_id, source` | `guild_id:text`, `member_id:text`, `source:text`, `last_awarded_at:timestamptz` |
| leveling / `xp_awards` | `id` | `id:bigint`, `guild_id:text`, `member_id:text`, `source:text`, `xp:integer`, `occurred_at:timestamptz`, `channel_id:text` |
| leveling / `level_role_rewards` | `guild_id, level` | `guild_id:text`, `level:integer`, `role_id:text` |
| leveling / `level_import_runs` | `id` | `id:bigint`, `guild_id:text`, `source:text`, `source_rows:integer`, `unique_members:integer`, `inserted:integer`, `updated:integer`, `unchanged:integer`, `duplicate_rows:integer`, `total_imported_xp:bigint`, `imported_at:timestamptz` |
| website_contract / `web_contract_meta` | `singleton` | `singleton:boolean`, `contract_version:text`, `guild_id:text` |
| website_contract / `guild_counters` | `guild_id` | `guild_id:text`, `human_member_count:integer`, `human_member_count_at:text`, `online_count:integer`, `online_count_at:text` |
| website_contract / `rank_ladder` | `rank_key` | `rank_key:text`, `rank_label:text`, `rank_order:integer`, `role_id:text` |
| website_contract / `rank_snapshots` | `guild_id, rank_key` | `guild_id:text`, `rank_key:text`, `member_count:integer`, `holders_count:integer`, `snapshot_at:text` |
| website_contract / `member_ranks` | `guild_id, member_id` | `guild_id:text`, `member_id:text`, `rank_key:text`, `updated_at:text` |
| website_contract / `scheduled_events` | `guild_id, event_id` | `guild_id:text`, `event_id:text`, `name:text`, `starts_at:text`, `channel_id:text`, `description:text`, `status:text`, `updated_at:text` |
| website_contract / `counter_snapshots` | `guild_id` | `guild_id:text`, `human_member_count:integer`, `human_member_count_at:text`, `online_count:integer`, `online_count_at:text` |
| website_contract / `member_exclusions` | `guild_id, member_id` | `guild_id:text`, `member_id:text`, `reason:text`, `updated_at:text` |
| presence / `presence_probe` | `guild_id, observed_at` | `guild_id:text`, `observed_at:text`, `approximate_presence_count:integer`, `bot_floor:integer`, `bot_floor_scan_truncated:boolean` |
| community / `community_facts` | `id` | `id:bigint`, `guild_id:text`, `event_type:text`, `source_event_id:text`, `actor_id:text`, `occurred_at:text`, `recorded_at:text`, `source:text`, `classifier_version:text`, `classification:text`, `matched_rule:text`, `metadata:text`, `idempotency_key:text` |
| community / `community_stream_heartbeats` | `guild_id, stream` | `guild_id:text`, `stream:text`, `covered_from:text`, `covered_through:text`, `updated_at:text` |
| community / `community_scorecard_runs` | `id` | `id:bigint`, `guild_id:text`, `week_start:text`, `week_end:text`, `classifier_version:text`, `watermark:bigint`, `input_count:integer`, `input_hash:text`, `idempotency_key:text`, `revision:integer`, `run_status:text`, `coverage_state:text`, `evidence_state:text`, `scorecard_json:text`, `intervention_code:text`, `generated_at:text` |
| community / `community_scorecard_alerts` | `guild_id, alert_key` | `guild_id:text`, `week_start:text`, `alert_key:text`, `created_at:text` |
| guild_settings / `guild_settings` | `guild_id, key` | `guild_id:text`, `key:text`, `value:jsonb`, `version:bigint`, `updated_at:timestamptz`, `updated_by:text` |
| guild_settings / `guild_settings_audit` | `id` | `id:bigint`, `guild_id:text`, `key:text`, `old_value:jsonb`, `new_value:jsonb`, `actor:text`, `at:timestamptz` |
| operational_audit / `operational_audit_log` | `entry_id` | `entry_id:text`, `event_kind:text`, `guild_id:text`, `occurred_at:timestamptz`, `actor_id:text`, `target_id:text`, `source_channel_id:text`, `destination_channel_id:text`, `message_id:text`, `action:text`, `metadata_json:text`, `created_at:timestamptz`, `mirror_channel_id:text`, `delivery_state:text`, `delivery_attempts:integer`, `delivery_attempted_at:timestamptz`, `delivery_last_error:text`, `delivery_lease_until:timestamptz`, `mirrored_at:timestamptz`, `delivery_nonce:text`, `mirror_message_id:text`, `delivery_search_before:text`, `delivery_claim_token:text`, `mirror_checked_at:timestamptz` |
| operational_audit / `audit_kill_switch` | `id` | `id:integer`, `engaged_at:timestamptz`, `engaged_by:text` |

## Pending and retired groups

| Group | Status / reason |
|---|---|
| `internal_actions` | **pending** — legacy 0002 raw key/nonce, result JSON and request-id log are incompatible with next 0350 hashed guards/scalar intent ledger; dedicated semantic migration required |
| `gateway_sessions` | **pending** — next 0320 exists, but legacy 0001–0042 has no Postgres gateway_sessions table; do not invent file-session conversion |
| `moderation` | **pending** — warnings/scheduled unbans lack target DDL; channel recovery uses new generations and requires a dedicated ledger mapping |
| `automations` | **pending** — commands/scheduled messages lack target DDL; sticky recovery needs source DDL and lease/generation mapping |
| `tickets` | **pending** — ticket/transcript target migrations are not shipped |
| `automod` | **pending** — automod/anti-nuke/containment target migrations are not shipped |
| `self_roles` | **pending** — self-role panel/audit/recovery target migrations are not shipped |
| `feeds` | **pending** — feed relay/delivery target migrations are not shipped |
| `invite_campaigns` | **pending** — invite_campaigns target migration is not shipped |
| `lfg_rsvp_temp_voice` | **pending** — LFG/RSVP need authoritative legacy runtime DDL; temp-voice target migration is not shipped |
| `onboarding_rota` | **retired** — rota is explicitly retired; no row-copy target |

Internal actions are intentionally **pending despite next 0350 existing**:
legacy 0002 raw keys/nonces, response JSON, request-ID logs and Discord event-ID
maps cannot truthfully become next's hashed, expiring, scalar intent ledger by
column casting. Hash namespaces, response semantics and intent relationships
need a separately reviewed semantic migration; never reset replay guards or
invent successful responses. Gateway sessions are pending because no table is
present in the pinned legacy Postgres migrations; file persistence is not a
Postgres source. No pending group is marked complete merely because its target
has a same-named table.

## Verification

```sh
cargo test -p two-bot-cutover --lib --bins --locked
cargo test -p two-bot-cutover --test legacy_copy_db --locked -- --ignored
cargo clippy -p two-bot-cutover --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

The ignored acceptance test uses the **existing fail-closed test-database guard**
and fixed `agent-testdb:5432`, `agent_test`, empty password. CI maps that hostname
to its disposable Postgres service. It never consults application/staging/production
URLs. It creates two uniquely named `two_bot_test_copy_*` databases, applies all
54 legacy fixture migrations and the complete next migration set, seeds every
mapped table, then compares **every mapped column**. It proves dry-run no mutation,
native numeric pagination, actual failure after a committed batch and recovery,
no-op replay (including settings revision), sequence preservation, append-only
refusal, all-selected schema/delivery preflight, ordinary updates, alternate-key
collision refusal, target-only defaults and preserved audit halt evidence.
Scenario assertion panics still run owned-database cleanup. The CI `check` job
runs this suite explicitly. Scratch databases and run-owned build scratch are
cleaned after tests; no real database, deployment or Discord journey is involved.

### Rehearsal test

`crates/cutover/tests/cutover_rehearsal.rs` chains the runbook order in one
test on two disposable `two_bot_test_rehearsal_*` databases: migrate the Next
schema, `legacy_copy` dry-run plan then apply, `legacy_verify`, a MEE6 import
from `fixtures/mee6_rehearsal_export.json`, then a full rerun. It asserts
planned, applied and verified counts agree per group, the rerun changes zero
rows, a one-row drift fails verification naming `events`, and pending groups
refuse loudly in both copy and verify instead of being omitted. Unlike the
ignored suites above it is **not** ignored: the CI `check` job's
`cargo test --workspace --test '*'` step runs it against the disposable
Postgres service, and missing/refused database access fails rather than
skipping. No network, no staging, no workflow change.
