# two-bot-next

[![Release](https://img.shields.io/github/v/release/TogetherWeOwn/two-bot-next)](https://github.com/TogetherWeOwn/two-bot-next/releases)

The Together We Own Discord bot (Owen), rewritten in Rust to run in a single always-on
Cloudflare Container. It replaces [two-bot](https://github.com/TogetherWeOwn/two-bot)
(TypeScript/discord.js), which is now in maintenance mode: fixes only, no new features.

Status: the bot has been live in production on one Cloudflare Container since
2026-10-10. A subset of commands is published; see the
[command publication guide](docs/command-publish.md) for the enabled registry
and its gates. Publication metadata is not proof that every feature is wired
into the runtime: remaining seams and drop decisions are tracked in the
[parity matrix](docs/parity.md). Twilight is the Discord framework — see
[ADR 0001](docs/adr/0001-discord-framework.md). The `two-bot` binary exposes
health/readiness, persists gateway sessions and funnel effects, and provides
backup/restore operator commands.

On-call: [operations runbook](docs/runbook.md) — health/logs, redeploy/rollback,
RESUME semantics, kill-switch/feature wiring boundaries, backups and common
failures. The [observability runbook](docs/runbook-observability.md) explains
alert triage, deployment-to-PR correlation, and current monitoring limits. See
also [backup procedures](docs/backup.md) and [staging soak](docs/staging-soak.md).

## Architecture

- **`two-bot-core` (`crates/core`)**: framework-free domain logic, compiled command
  definitions/router, settings classification and feature storage interfaces.
  Optional `db` stores use sqlx; pure domain tests need no database.
- **`two-bot-discord` (`crates/discord`)**: Twilight gateway/REST adapters, event
  normalization, gateway recovery and action execution. See the
  [framework decision](docs/adr/0001-discord-framework.md).
- **`two-bot` (`crates/bot`)**: executable Container service, gateway supervision,
  readiness endpoints and runtime wiring.
- **`two-bot-cutover` (`crates/cutover`)**: migration/backup helpers and shared
  settings persistence for the cutover.
- **`wrangler/`**: Cloudflare Worker + Durable Object wrapper routes HTTP to a
  singleton `lite` Container and keeps it alive on a 60-second schedule. The Rust
  process owns the long-lived Discord gateway connection; the Worker does not.
- **Shared Postgres (PlanetScale in production, Neon for staging)**: the bot and two-web-next use the shared database
  contract. Schema/cutover work lives in `sql/` and the cutover crate, not a second
  isolated website datastore.

The `lite` design target remains one gateway session and less than 256 MiB RSS.
This placement/deployment reference is not B2 soak acceptance: B2 requires flat
memory and defines no numeric RSS threshold. This README makes no measurement
claim. Staging deploys from `main`; production deploys through the manual
[`deploy-production` gate](docs/production-deploy.md).

## Operator references

Browse the [documentation map](docs/README.md) for every guide, contract and evidence form.

- [Commands](docs/commands.md): built-ins, options, registry bounds and default
  Discord permissions, rendered from the compiled all-enabled registry.
- [Event help and attendee reminders](docs/event-reminders.md): fixed 15-minute,
  channel-only reminders, delivery limits and restart-safe failure behavior.
- [Proposed RSVP seats and waitlist](docs/rsvp-capacity-spec.md): design only;
  optional Going capacity, explicit queue consent, race/privacy contracts and
  unresolved existing-post assumptions. Not an enabled feature.
- [Sticky delivery recovery](docs/sticky-debounce-acceptance.md#uncertain-delivery-recovery):
  inspect uncertain re-posts before resetting the durable delivery fence.
- [Configuration](docs/configuration.md): all catalog keys/classes, known parsed
  defaults and descriptions. Classes are legacy-catalog labels only: gateway
  boot reads a fixed set of process-environment keys, and the Container's
  `guild_settings` job (`crates/bot/src/website_jobs.rs:207`) polls every 15 s
  and publishes a snapshot only when the revision or row count changes
  (`crates/bot/src/settings_jobs.rs:146-160`), exposed through `settings_jobs::live`
  (`crates/bot/src/settings_jobs.rs:53`); the live-snapshot consumer table in
  the reference names every reader — automod, feed, audit, ticket, gateway
  classifier and voice — alongside the per-runtime store and per-event
  refresh paths (re-checked at `1643438`).
- [Hot-path read bounds](docs/store-cutover.md#hot-path-store-read-bounds): scoped settings, 25-row
  decay/retention passes, burst inspection and complete paged membership.
- [Feed supervisor](docs/feed-supervisor.md): running feed delivery —
  source configuration, fetch cadence as-coded, failure signals and triage,
  read-only guarantees, and worked examples.
- [Gateway recovery runbook](docs/gateway-recovery.md): readiness, durable RESUME,
  failure handling and rollback.
- [Staging soak acceptance](docs/staging-soak.md): the evidence required before
  cutover; deployment alone is not acceptance.
- [Staging slash smoke](docs/staging-slash-smoke.md): fixture-only router/reply
  checks, plus a fenced GET-only live smoke run (health, build identity, guild
  command registry) that writes a run record; neither invokes a slash command.
- [Backup/restore runbook](docs/backup.md) and [parity matrix](docs/parity.md).
- [Channel lane reconciliation](docs/channel-lane-reconciliation.md): inspection-first
  operator release of an uncertain moderation lane, with explicit confirmation,
  generation fencing, atomic audit and preserved lockdown recovery.
- [Command registry drift](docs/command-publish.md): dry-run-first
  `two-bot commands diff|publish` and opt-in boot publication for cutover.
- [Game-activity aggregate report](docs/game-activity-report.md): private staff
  `/game-mix` and read-only per-game play/in-voice/session CLI JSON for one guild;
  Discord reads remain unavailable in the live guild.
- [Automod operator guide](docs/automod-operator-guide.md): filter/effect
  semantics, repeat expiry, corpus management, failure signals and triage.

### Stream delivery (Twitch EventSub)

Twitch `stream.online` deliveries arrive at the staging-only public Worker
route (`POST /hooks/twitch`, `wrangler/src/eventsub.ts:37`), which forwards
them inward (`wrangler/src/eventsub.ts:289`) to the private container
receiver (`crates/bot/src/eventsub_http.rs:1`). The Worker route answers only
when the staging-only Worker var `EVENTSUB_INGRESS` and the Operator-set Worker
secret `TWO_EVENTSUB` are both exactly `1`. A missing `TWO_TWITCH_WEBHOOK_SECRET`
leaves the route live but forwards nothing, so it responds `503 unavailable`;
flags other than `1` keep the route dark. The container listener on a direct
start requires `TWO_EVENTSUB` exactly `1`, a valid bind with a nonzero port, and
a valid webhook secret (`crates/core/src/eventsub_receiver_config.rs:5`). On a
direct container start, unset or `0` stays dark; any other value, or an enabled
configuration with a missing or invalid bind or secret, stops startup. Through
the Worker, a present-but-invalid secret stops container startup. There is no
health-only fallback; see the [EventSub runbook](docs/eventsub-runbook.md).
Every delivery body is capped at 64 KiB and verified
(`twitch_eventsub::verify_delivery`,
`crates/core/src/twitch_eventsub.rs:169`) before JSON parsing or classification
(`classify`, `crates/core/src/twitch_eventsub.rs:261`). Only `stream.online`
version 1 is accepted for classification. With delivery dark the runtime
receiver only acknowledges verified notifications and revocations; with
delivery enabled a verified `stream.online` runs the durable claim → send →
settle flow (`deliver_go_live`, `crates/bot/src/eventsub_http.rs:282`;
production deps built at `crates/bot/src/main.rs:437`). Planning rules live in the
[stream funnel](docs/stream-funnel.md); the operator runbook is the
[EventSub runbook](docs/eventsub-runbook.md). The keys (`TWO_EVENTSUB`,
`TWO_EVENTSUB_BIND`, `TWO_TWITCH_WEBHOOK_SECRET`) are catalogued in
[Configuration](docs/configuration.md) (`docs/configuration.md:200`).

### Regenerate the references

The database-free integration test `crates/core/tests/reference_docs.rs` renders
both references. It uses compiled registry/catalog data and empty-map typed
loaders, never live environment values or IDs. The classification-only settings
catalog has no descriptions; its checked supplement lives in
`crates/core/tests/fixtures/reference_settings.json`. Adding/removing a catalog
key requires updating that supplement. Defaults without a typed-loader mapping
are explicitly marked **Not specified in Next**, not guessed from legacy.

The command rows are no longer committed. The authored
[command-reference landing page](docs/commands.md) points to the ignored
`docs/generated/commands.md` and the **command-reference** artifact on each
Rust-selected check run. Generate explicitly before compiling workspace tests or
all-targets Clippy. The generator validates tracked configuration first and
never rewrites it or the saved publication fixtures.

On the persistent controller, after bounded pool admission:

```sh
python3 scripts/generate_command_reference.py --controller
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test reference_docs
```

Outside the controller, use `python3 scripts/generate_command_reference.py`, then
`cargo test -p two-bot-core --test reference_docs --locked`. The script builds the
small core example that reuses the canonical renderer; no database, deployment
or live environment values are needed.

Configuration remains tracked. When deliberately changing its catalog or
supplement, explicitly regenerate both references with
`cargo test -p two-bot-core --test reference_docs regenerate_reference_docs -- --ignored`
(put the Cargo arguments through `scripts/cargo_cache.py` on the controller).
Review and commit only the configuration change, never generated command rows.
Ordinary reference tests compare bytes and never regenerate silently: absent,
stale or malformed generated output fails, and configuration drift remains a
failure. CI generates before consumers, then runs these read-only tests.

## Contributing

Squash-merge only; PR titles follow Conventional Commits and the body follows the
[PR template](.github/pull_request_template.md). This repo is public, so keep
internal tracker IDs out of titles, bodies, commits and branch names. The rulesets
require `ci-ok`, `gitleaks` and `pr-lint` on the exact head commit. The single
`ci-ok` aggregate gates both selectors, lint (`check`: fmt, clippy -D warnings,
cargo-deny), all selected `rust tests` and `ignored db` lanes, and `worker check`
(including the runbook command-drift test). The approving non-author reviewer
squash-merges. See [Contributing](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md). The
workspace toolchain is pinned in `rust-toolchain.toml`.

Persistent-controller Rust builds use the [bounded Cargo cache wrapper](docs/build-cache.md),
not a new `target/` in each worktree. The runbook includes offline cache tests,
fail-closed retention auditing, `/home` available-byte alarms and the Operator rollout.

## Operations

- [Production cutover and rollback](docs/cutover.md): B4 preconditions, freeze/drain,
  data and command-registry checks, 48-hour watch and preservation of Next-window
  writes on rollback. Planned tools are explicitly marked; this is not cutover approval.
- [Staging soak](docs/staging-soak.md) and [gateway recovery](docs/gateway-recovery.md).
- [Weekly DORA metrics](docs/dora-metrics.md): change failure rate, recovery
  time, merge-to-prod lead time and throughput, committed to `ops/metrics/`
  on the `automation/dora-metrics` branch.

Migration changes must pass the [numbering and checksum guard](docs/migrations.md).
Add new SQL files and their `migrations.lock` entries together; existing migration
edits require a fresh justification against the PR base, not just a new checksum.

## License

Business Source License 1.1, converting to MIT three years after each release. See [LICENSE](LICENSE).
