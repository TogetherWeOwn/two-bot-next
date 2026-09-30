# two-bot-next

The Together We Own Discord bot (Owen), rewritten in Rust to run in a single always-on
Cloudflare Container. It replaces [two-bot](https://github.com/TogetherWeOwn/two-bot)
(TypeScript/discord.js), which is now in maintenance mode: fixes only, no new features.

Status: scaffold (S1). Twilight is the chosen Discord framework — see
`docs/adr/0001-discord-framework.md`. The workspace (`crates/core`,
`crates/discord`, `crates/bot`), CI `check` job, `Dockerfile` and the
`wrangler/` Container + Worker/DO wrapper are in place; the gateway
supervisor connects in S3.

Targets: `lite` Container instance (under 256 MiB RSS), one gateway session, shared
Postgres with two-web-next. Migration plan: TOG-9671.

## Contributing

Squash-merge only; PR titles follow Conventional Commits and the body carries
`Refs: TOG-1234`. `gitleaks` and `pr-lint` are required; `check` (fmt, clippy, test)
becomes required with the first Rust code.

Persistent-controller Rust builds use the [bounded Cargo cache wrapper](docs/build-cache.md),
not a new `target/` in each worktree. The runbook includes offline cache tests,
fail-closed retention auditing, `/home` available-byte alarms and the Operator rollout.

## License

Business Source License 1.1, converting to MIT three years after each release. See [LICENSE](LICENSE).
