# two-bot-next

[![Release](https://img.shields.io/github/v/release/TogetherWeOwn/two-bot-next)](https://github.com/TogetherWeOwn/two-bot-next/releases)

The Together We Own Discord bot (Owen), rewritten in Rust to run in a single always-on
Cloudflare Container. It replaces [two-bot](https://github.com/TogetherWeOwn/two-bot)
(TypeScript/discord.js), which is now in maintenance mode: fixes only, no new features.

Status: Rust gateway and durable recovery implemented; feature/core and storage
ports are in place, with runtime integration still incomplete. Twilight is the
Discord framework — see [ADR 0001](docs/adr/0001-discord-framework.md). The
`two-bot` binary exposes health/readiness, persists gateway sessions and funnel
effects, and provides backup/restore operator commands. The `wrangler/` wrapper
targets one Cloudflare Container; this is not a production-cutover or soak claim.
Command publication metadata is not proof that every feature is wired into the
runtime: remaining seams and drop decisions are tracked in the
[parity matrix](docs/parity.md).

On-call: [operations runbook](docs/runbook.md) — health/logs, redeploy/rollback,
RESUME semantics, kill-switch/feature wiring boundaries, backups and common
failures. See also [backup procedures](docs/backup.md) and
[staging soak](docs/staging-soak.md).

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
- **Shared Neon Postgres**: the bot and two-web-next use the shared database
  contract. Schema/cutover work lives in `sql/` and the cutover crate, not a second
  isolated website datastore.

Targets remain one gateway session and less than 256 MiB RSS on the `lite`
Container; these are deployment/soak gates, not a claim of measurements in this
README. Staging deploys from `main`; production is a separate manual gate.

## Operator references

- [Commands](docs/commands.md): built-ins, options, registry bounds and default
  Discord permissions, rendered from the compiled all-enabled registry.
- [Configuration](docs/configuration.md): all catalog keys/classes, known parsed
  defaults, descriptions and hot/cold/env_only classes. Hot is a legacy-catalog
  reload classification only; the Container currently wires no settings
  store/poller/consumer, so every key is restart-applied (see the reference).
- [Gateway recovery runbook](docs/gateway-recovery.md): readiness, durable RESUME,
  failure handling and rollback.
- [Staging soak acceptance](docs/staging-soak.md): the evidence required before
  cutover; deployment alone is not acceptance.
- [Backup/restore runbook](docs/backup.md) and [parity matrix](docs/parity.md).

### Regenerate the references

The database-free integration test `crates/core/tests/reference_docs.rs` renders
both references. It uses compiled registry/catalog data and empty-map typed
loaders, never live environment values or IDs. The classification-only settings
catalog has no descriptions; its checked supplement lives in
`crates/core/tests/fixtures/reference_settings.json`. Adding/removing a catalog
key requires updating that supplement. Defaults without a typed-loader mapping
are explicitly marked **Not specified in Next**, not guessed from legacy.

On the persistent controller, after the bounded pool is admitted:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test reference_docs regenerate_reference_docs -- --ignored
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test reference_docs
```

On ephemeral/local developer machines outside the controller, the equivalent is
`cargo test -p two-bot-core --test reference_docs regenerate_reference_docs -- --ignored`,
then `cargo test -p two-bot-core --test reference_docs`. Ordinary tests compare
committed files byte-for-byte and never regenerate them silently. CI discovers
these tests in its existing integration-test step.

## Contributing

Squash-merge only; PR titles follow Conventional Commits and the body carries
`Refs: TOG-1234`. Required exact-head checks include `gitleaks`, `pr-lint`,
`check` (fmt, clippy -D warnings, tests, cargo-deny) and `worker check` (including
the runbook command-drift test). The approving non-author reviewer squash-merges.
See [Contributing](CONTRIBUTING.md). The workspace toolchain is pinned in
`rust-toolchain.toml`.

Persistent-controller Rust builds use the [bounded Cargo cache wrapper](docs/build-cache.md),
not a new `target/` in each worktree. The runbook includes offline cache tests,
fail-closed retention auditing, `/home` available-byte alarms and the Operator rollout.

## License

Business Source License 1.1, converting to MIT three years after each release. See [LICENSE](LICENSE).
