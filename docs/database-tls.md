# Database TLS policy

Threat-model [F6](threat-model.md) requires authenticated TLS for Neon. The
`two_bot_core::database_tls` module fences the database URL before SQLx parses
it. `two_bot_cutover::connect` (cutover CLIs, `two-bot db roles verify` and the
bot's website/community job pool) calls it after the `database_url` key
allowlist and before `connect_options`. The three bot-side send-admission pools
share one helper
(`website_jobs::admission_pool_with_tls`, used by the website jobs, the preflight
`admission_transport` and the commands CLI `executor`). The cutover tools' fourth
pool (`RestClient::from_env`) uses its own `admission_connect_options` helper.
Both paths validate before enforcing TLS, then parse and apply the effective TLS
mode; each configures the statement and acquire timeouts.

## Setting

`TWO_DATABASE_TLS` selects the policy. It has two values:

| Value | Policy | Who sets it |
| --- | --- | --- |
| unset or `required` | `Required` | Default. Staging and production never set it. |
| `local-only` | `LocalOnly` | Tests and CI only, explicitly. |

Any other value, including an empty or non-UTF-8 one, refuses the connection.
The Worker forwards `DISCORD_TOKEN`, `DATABASE_URL` and `GUILD_ID` plus the
reviewed `TWO_*` flag allowlist (`forwardedFlagVars`), `DISCORD_APPLICATION_ID`,
the lobby channel, `LISTEN_ADDR` and the private-receiver lines into the
container (`containerEnvVars` in `wrangler/src/index.ts:271-310`, allowlist in
`wrangler/src/container-env.ts`). `TWO_DATABASE_TLS` is explicitly not
forwarded (it stays in `NOT_FORWARDED`), so the deployed bot never sees it and
always runs `Required`.
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
`open_pool_with_tls`, and `governed_guild_config_api`),
`channel_moderation_store::connect` (via `connect_with_tls`), and all four
send-admission pools (`website_jobs::admission_pool_with_tls` for the website
jobs, the preflight `admission_transport`, the commands CLI `executor`, and
`two_bot_cutover::rest::RestClient::from_env` via `admission_connect_options`
(with `from_url_with_tls` as the explicit URL/policy test constructor) for the
`report`, `ghost_cleanup` and `backfill_messages` operator tools).

Known gaps (not yet fenced): `staging_migrate::verify_target` plus `connect`
(`crates/cutover/src/staging_migrate.rs`) pins the expected host and database
but never calls `database_tls::enforce`/`apply` and sets no timeouts; the
`legacy_copy` binary (`crates/cutover/src/bin/legacy_copy.rs`) builds its
source/target pools with raw `PgPoolOptions::connect_with` from operator URLs
without calling `database_tls::enforce`/`apply`; and the `legacy_verify`
binary (`crates/cutover/src/bin/legacy_verify.rs`) builds its source/target
`PgConnectOptions` via `connection_options` from operator URLs without calling
`database_tls::enforce`/`apply`. Their
refusals already use fixed strings with no URL, host or credential.

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
  `secret_redaction::channel_store_tls_refusals_never_echo_urls_or_reach_logs`,
  `rest::admission_tls_fence_refuses_plaintext_and_wrong_hosts` plus the
  `from_env` entry proofs
  (`admission_configuration::admission_bootstrap_tls_refusal_redacts_dependency_logs`
  for a realistic remote URL, and the dial-discriminating
  `admission_bootstrap_tls_spy_refuses_before_any_socket`, which fails when
  the fence is reverted to raw `connect_options`).
- DB suites and CLIs pass `LocalOnly` explicitly (`connect_with_tls` /
  `connect_pool_with_tls` / `open_pool_with_tls`, or
  `TWO_DATABASE_TLS=local-only` on `env_clear()` subprocesses). The CI `check`
  job and the nightly `sweep` job set it for in-process callers.
