# two-bot-next

The Together We Own Discord bot (Owen), rewritten in Rust to run in a single always-on
Cloudflare Container. It replaces [two-bot](https://github.com/TogetherWeOwn/two-bot)
(TypeScript/discord.js), which is now in maintenance mode: fixes only, no new features.

Status: framework selection. The Rust Discord library (for example twilight or
serenity/poise) is being chosen on its own research card before any code lands. The
decision record goes in `docs/adr/0001-discord-framework.md`.

Targets: `lite` Container instance (under 256 MiB RSS), one gateway session, shared
Postgres with two-web-next. Migration plan: TOG-9671.

## Contributing

Squash-merge only; PR titles follow Conventional Commits and the body carries
`Refs: TOG-1234`. `gitleaks` and `pr-lint` are required; `check` (fmt, clippy, test)
becomes required with the first Rust code.

## License

Business Source License 1.1, converting to MIT three years after each release. See [LICENSE](LICENSE).
