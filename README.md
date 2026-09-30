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

On-call: [operations runbook](docs/runbook.md) — health/logs, redeploy/rollback,
RESUME semantics, kill-switch/feature wiring boundaries, backups and common
failures. See also [backup procedures](docs/backup.md) and
[staging soak](docs/staging-soak.md).

Targets: `lite` Container instance (under 256 MiB RSS), one gateway session, shared
Postgres with two-web-next. Migration plan: TOG-9671.

## Contributing

Squash-merge only; PR titles follow Conventional Commits and the body carries
`Refs: TOG-1234`. Required exact-head checks include `gitleaks`, `pr-lint`,
`check` (fmt, clippy -D warnings, tests, cargo-deny) and `worker check` (including
the runbook command-drift test). The approving non-author reviewer squash-merges.

Persistent-controller Rust builds use the [bounded Cargo cache wrapper](docs/build-cache.md),
not a new `target/` in each worktree. The runbook includes offline cache tests,
fail-closed retention auditing, `/home` available-byte alarms and the Operator rollout.

## Operations

- [Production cutover and rollback](docs/cutover.md): B4 preconditions, freeze/drain,
  data and command-registry checks, 48-hour watch and preservation of Next-window
  writes on rollback. Planned tools are explicitly marked; this is not cutover approval.
- [Staging soak](docs/staging-soak.md) and [gateway recovery](docs/gateway-recovery.md).

## License

Business Source License 1.1, converting to MIT three years after each release. See [LICENSE](LICENSE).
