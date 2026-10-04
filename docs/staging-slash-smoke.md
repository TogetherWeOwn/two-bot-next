# Offline staging informational-command smoke

The test-support runner in `crates/discord/src/staging_slash_smoke.rs` is the
code foundation for staging smoke, **not Discord E2E or a staging PASS**. It has
no HTTP client, credential loader, command publisher, environment lookup or CLI.
It is absent from normal release builds; it is available only to tests or through
the default-off `test-support` feature.

Use local `SmokeFixtures` and a fresh `SmokeTransports` factory for each run.
The factory returns a new in-memory `ReplyTransport` for each distinct local
slash interaction identity; never reuse one interaction's transport across
commands. Health and skipped commands create no transport. There is no
live adapter in this slice. A request setting `live_execution` is refused, not an
opt-in. Do not connect `InteractionReplyTransport`, automate a Discord user token,
or submit invented interactions to a Discord endpoint. Real slash-command E2E
requires supported Discord user interaction and separate staging execution gates.

## Explicit fence, before all fixture/transport calls

`SmokeConfig` requires the exact staging application and guild. It reuses the
public identity constants in `two_bot_core::backup::guild_config`; it does not
introduce a second identity list. The live guild is refused first, including when
other configuration is absent or live execution was requested. Unknown, missing,
zero, whitespace-padded, leading-zero and wrong-application identities refuse
without reading a fixture or issuing a callback. Default configuration is missing
identities and live execution is disabled. There is no live-guild override.

Rank observations must use the canonical guild, XP within the existing storage
ceiling, and the compiled XP curve's level and next-level floor. Validation
bounds XP before curve evaluation and rejects arbitrary/overflowing fixture
levels without rendering a reply or creating a rank transport. Bad models leave
a failed receipt and do not prevent the remaining steps from being recorded.

The per-step deadline must be greater than zero and at most five seconds. The
runner awaits each step sequentially and drops a timed-out future; it never
spawns detached work or blindly retries a possibly delivered callback. A failed
or timed-out rank ACK is not reused by the leaderboard's separate transport.
As with other async deadlines, fixture implementations must yield rather than
block, and transport factories must remain lightweight and local.

## Honest scope

| Plan surface | Offline behavior | What is not proved |
| --- | --- | --- |
| `/rank` | Checks the compiled publish set and actual router handler; builds the fixture reply with `rank_reply`; runs the shared reply dispatcher | PostgreSQL reads, actual gateway dispatch, deployed reply delivery |
| `/leaderboard` | Same route/dispatcher path with `leaderboard_reply`; covers populated and empty fixtures and mention suppression | Live leaderboard state or Discord receipt |
| `/help` | Records `skipped / unsupported-core-command`; no fabricated interaction or fixture call | A built-in `/help` is absent from this revision |
| `/ping` | Records `skipped / voice-command-out-of-scope`; no fixture call | Voice-family ping gates and latency behavior |
| health/readiness | Classifies local liveness/readiness snapshots; healthy but unready fails | No HTTP endpoint is requested or measured |

Success for the supported fixtures is **`incomplete`**, not PASS, because the
older plan's help/ping coverage is still missing. Any down, timeout, mismatched
fixture or callback failure produces `fail`. Invalid configuration produces
`refused` with no command entries. The source staging acceptance must resolve
those scope gaps and obtain real deployed evidence separately.

## Redacted partial receipt

`SmokeReport` serializes only `mock: true`, `transport: local-fixtures`,
`live_execution: false`, bounded classifications, fixed surface names, whole
monotonic `duration_ms` values and an offline verdict. It never includes reply
content, member/application/guild identities, tokens, raw errors or configuration
values. Failure entries carry a constant offline failure signature.

The `name`, `duration_ms`, `result` (`pass`, `fail`, `skipped`), `actual` and
`failure_signature` vocabulary follows the
[staging run-record contract](staging-e2e-run-record.schema.json). This is a
**partial offline receipt**, not a schema-complete E2E record: no UTC invocation
time, deployment revision or deploy run ID is invented. Do not populate missing
deployment facts with dummy IDs or offer this receipt to the validator as live
acceptance evidence. The existing mock-record rejection remains unchanged.

## Verification

On the persistent controller:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --lib staging_slash_smoke::tests
```

CI discovers these unit tests in the existing library-test lane. No workflow,
credential, service container or database is needed. The fixture pack is
`crates/discord/tests/fixtures/staging_slash_smoke.json`.

Must-pass cases: fixture XP/level consistency with the compiled curve;
populated/empty replies; live and bad-config zero-call refusal;
live-mode refusal; down and healthy-but-unready health; source and callback
timeouts; dropped pending futures; callback errors without retries; wrong read
model/foreign rank model; inconsistent and extreme numeric rank models; the
maximum supported XP boundary; strict one-ACK transport scopes and isolated
rank timeout/failure; sensitive-data exclusion from JSON and Debug; and
independent simultaneous offline runs. Local timings are fixture timings, not
production performance measurements.
