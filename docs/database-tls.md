# Database TLS policy

Threat-model [F6](threat-model.md) requires authenticated TLS for Neon. The
`two_bot_core::database_tls` module fences the database URL before SQLx parses
it. `two_bot_cutover::connect` (cutover CLIs, `two-bot db roles verify` and the
bot's website/community job pool) calls it after the `database_url` key
allowlist and before `connect_options`.

## Setting

`TWO_DATABASE_TLS` selects the policy. It has two values:

| Value | Policy | Who sets it |
| --- | --- | --- |
| unset or `required` | `Required` | Default. Staging and production never set it. |
| `local-only` | `LocalOnly` | Tests and CI only, explicitly. |

Any other value, including an empty or non-UTF-8 one, refuses the connection.
The Worker forwards only `DISCORD_TOKEN`, `DATABASE_URL` and `GUILD_ID` to the
container (`wrangler/src/index.ts`), so the deployed bot always runs `Required`.
The setting is not a dashboard setting: `settings::classify_key` refuses
unknown names.

## Host classes

The check counts every host the driver could use: the URL authority and each
`host`/`hostaddr` query value. SQLx lets a query value override the authority,
so every one of them must pass. A URL with no host is refused, because SQLx
would fall back to `PGHOST` or a local default.

| Class | Examples | `Required` | `LocalOnly` |
| --- | --- | --- | --- |
| Remote | `ep-<id>.<region>.aws.neon.tech`, `203.0.113.7`, `[2001:db8::7]`, any dotted name | allowed | refused |
| Loopback | `localhost`, `127.0.0.0/8`, `[::1]`, `[::ffff:127.0.0.1]` | refused | allowed |
| CI service | one DNS label starting with a letter: `agent-testdb`, `postgres` | refused | allowed |
| Unix socket | `%2Fvar%2Frun%2Fpostgresql` authority, `host=/path` | refused | allowed |

Digit-led single labels (`2130706433`, `0x7f000001`) are Remote, because
resolvers read them as IPv4 addresses. `LocalOnly` therefore cannot be used to
reach a remote host in plaintext. A single-label name is trusted as a
container-network service; DNS search domains could still expand it, which is
why `LocalOnly` is test-only.

## sslmode

Every `sslmode`/`ssl-mode` occurrence counts, case-insensitively, as in SQLx.

| sslmode | `Required` (remote host) | `LocalOnly` (local host) |
| --- | --- | --- |
| missing | refused | allowed |
| `disable`, `allow`, `prefer` | refused | allowed |
| `require`, `verify-ca`, `verify-full` | allowed, connects as `verify-full` | allowed, as written |
| anything else | refused | refused |

### Why `Required` always connects as `verify-full`

Decision for F6: under `Required`, the effective mode is `verify-full`,
whatever the URL spells (`database_tls::apply`).

- The lockfile pins `sqlx-postgres 0.9.0` with `tls-rustls-ring-webpki`. In
  `src/connection/tls.rs`, `require` sets `accept_invalid_certs` and
  `verify-ca` sets `accept_invalid_hostnames`. With rustls, `require` installs
  a verifier that accepts any certificate. That is encryption without server
  authentication, so an on-path attacker can terminate TLS.
- Neon's default URL is `sslmode=require&channel_binding=require`. With libpq,
  SCRAM channel binding would authenticate the server. SQLx does not implement
  channel binding, and PR #156 strips the parameter before SQLx. It gives no
  protection here.
- `verify-full` checks the chain against the bundled `webpki-roots` (ISRG Root
  X1 is included) and checks the hostname against the URL host. A custom
  `sslrootcert` adds a trust anchor. It does not disable verification.

Operators can keep Neon's copied URL. Don't add `sslrootcert=system`: SQLx
reads it as a file path and the connection fails.

Errors are fixed strings, such as `database sslmode does not require TLS`. They
never include the URL, host or credential. A refusal happens before SQLx parses
the URL or opens a socket. `crates/cutover/tests/secret_connection.rs` asserts
that nothing reaches logs.

## Coverage and gaps

Fenced: every caller of `two_bot_cutover::connect`, the gateway store pool
(`two_bot_store::connect_pool`, via `connect_pool_with_tls`), both
`two-bot backup` URL parses (`backup_cli::open_pool`, via
`open_pool_with_tls`, and `governed_guild_config_api`), and
`channel_moderation_store::connect` (via `connect_with_tls`).

F6 stays open until the deployment card records a non-secret TLS receipt.

## Tests

- `crates/core/src/database_tls.rs` contains table tests over every sslmode ×
  policy × host class. They also cover query-host overrides, repeated and
  percent-encoded keys, the Neon default URL and the effective SQLx mode.
- Each fenced path has a refusal proof mirroring
  `crates/cutover/tests/secret_connection.rs`: a remote `sslmode=disable` URL
  (and the other refusal cases) fails with the same fixed string before SQLx
  parses the URL or opens a socket, with no URL part in the error or the logs
  (`crates/store/tests/tls_refusal.rs`,
  `backup_cli::open_pool_with_tls_refusals_never_echo_urls_or_reach_logs`,
  `backup_cli::tls_admission_guard_redacts_dependency_logs`,
  `secret_redaction::channel_store_tls_refusals_never_echo_urls_or_reach_logs`).
- DB suites and CLIs pass `LocalOnly` explicitly (`connect_with_tls` /
  `connect_pool_with_tls` / `open_pool_with_tls`, or
  `TWO_DATABASE_TLS=local-only` on `env_clear()` subprocesses). The CI `check`
  job and the nightly `sweep` job set it for in-process callers.
