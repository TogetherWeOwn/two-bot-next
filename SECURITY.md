# Security Policy

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Report privately via
[GitHub private vulnerability reporting](https://github.com/TogetherWeOwn/two-bot-next/security/advisories/new)
(Security tab → Report a vulnerability). Only maintainers see the report, and
we will coordinate the fix and disclosure with you.

## Secret handling

Store credentials and credential-bearing URLs in `two_bot_core::Secret<T>`.
`Debug` (including pretty/nested output) and `Display` always produce
`[REDACTED]`, without invoking the inner formatter. There is no implicit
`Deref`, `AsRef`, or serialization. Call `expose()` only at the signing,
authorization-header, connection, or transport boundary; never log its result.
Full URL redaction is intentional: userinfo, query parameters and Discord
webhook paths can all contain credentials. This does not change secret storage,
rotate keys, encrypt memory, or provide zeroization.

The audited holders are `Config` (Discord token/database URL), `SigningKey`
and `KeyRing` (internal-action HMAC keys/decoy), the moderation-audit secret
loader, `S3Target`, `SignedRequest` (Authorization headers),
`GuildConfigDiscordApi`, and `HyperTransport` (also nested in `ActionExecutor`).
Ownership capabilities in `AuditClaim` and `ChannelClaimTicket` are redacted too.
`AuthDecision` already formats only body field counts, never OAuth payloads.
No production webhook client currently exists; new webhook URLs must use
`Secret<String>` too.

Do not print raw sqlx connection errors, parser errors, task panics, or remote
HTTP bodies. They can echo credentials. Database connection failures expose
bounded constant messages. Unsupported database URL query keys are rejected
before the pinned SQLx parser can WARN-log their values; re-audit the allowlist
in `database_url.rs` on SQLx upgrades. Database URLs are parsed through
`database_url::connect_options`, which also suppresses the driver's passfile
target for the synchronous parse: a malformed pgpass line can carry a
credential, while a well-formed entry still supplies the password exactly as
`FromStr` would. Re-audit the suppressed target (`sqlx_postgres::options::pgpass`
on pinned sqlx-postgres 0.9) on SQLx upgrades. The backup/restore CLI opens
pools only through its `open_pool` wrapper over the same path, so every CLI
failure is a bounded constant too. HTTP errors protect URLs/reasons/details,
response Debug shows only status/lengths, rejected Discord writes expose
only status/context, not remote JSON, and non-image emoji captures keep only
the constant classification, never the remote-controlled Content-Type value.
Successful S3 uploads never log remote
ETags. Transport URLs and Discord proxy overrides reject userinfo before Hyper
can DEBUG-log an authority/pool key; proxy overrides accept origins only, not
paths or queries. Truncation alone is not redaction. Do not enable
dependency TRACE logging of request payloads or format wire bodies/headers,
`PgConnectOptions`, or explicitly exposed values.

Run `python3 scripts/check-secret-debug.py` (and `--test` for its tripwire tests).
The required `check` CI job runs this grep-style guard over all Rust crate source
files, refusing derived Debug with raw string/byte credential fields. Debug in
any of several `#[derive]` attributes on one type counts, in either order and
with other attributes between them. Exact
`file/type/field` exceptions cover only public correlation/idempotency tokens.
The guard is deliberately not a Rust parser or data-flow analysis: unusual type
aliases, tuple structs, generic maps and custom formatters still require review.
Add a normal/pretty Debug sentinel regression for each new secret-bearing type;
the core `secret_redaction` suite and per-crate tests exercise existing holders.
Use synthetic fixtures only, never production credentials or databases.

## Feature wiring gates

Runtime wiring for vote-kick, `/templateassistant`, RSVP and attendance must
supply exact-head code and regression-test evidence for the applicable
[command wiring security requirements](docs/command-wiring-security.md).
Publication, core-only tests and documentation alone do not satisfy those gates.
Unresolved controls must stay explicit in each wiring or hardening PR.

## Supported Versions

Security fixes land on `main` and ship with the next release-please release
(see [CHANGELOG.md](CHANGELOG.md)). Pre-`1.0.0` versions are pre-production;
upgrade to the latest tagged release.
