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
bounded constant messages; HTTP errors protect URLs/reasons/details and response
Debug shows only status/lengths. Truncation alone is not redaction. Do not enable
dependency TRACE logging of request payloads or format wire bodies/headers,
`PgConnectOptions`, or explicitly exposed values.

Run `python3 scripts/check-secret-debug.py` (and `--test` for its tripwire tests).
The required `check` CI job runs this grep-style guard over all Rust crate source
files, refusing derived Debug with raw string/byte credential fields. Exact
`file/type/field` exceptions cover only public correlation/idempotency tokens.
The guard is deliberately not a Rust parser or data-flow analysis: unusual type
aliases, tuple structs, generic maps and custom formatters still require review.
Add a normal/pretty Debug sentinel regression for each new secret-bearing type;
the core `secret_redaction` suite and per-crate tests exercise existing holders.
Use synthetic fixtures only, never production credentials or databases.

## Supported Versions

Security fixes land on `main` and ship with the next release-please release
(see [CHANGELOG.md](CHANGELOG.md)). Pre-`1.0.0` versions are pre-production;
upgrade to the latest tagged release.
