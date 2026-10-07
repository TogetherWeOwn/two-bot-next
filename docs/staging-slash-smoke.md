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
| `/help` | Records `skipped / covered-by-offline-tests`; no fabricated interaction or fixture call | Live picker listing: `/help` is a compiled core command answered from the live publish set, pinned by offline router and renderer tests rather than this fixture smoke |
| `/ping` | Records `skipped / voice-command-out-of-scope`; no fixture call | Voice-family ping gates and latency behavior |
| health/readiness | Classifies local liveness/readiness snapshots; healthy but unready fails | No HTTP endpoint is requested or measured |

Success for the supported fixtures is **`incomplete`**, not PASS, because the
older plan's help/ping coverage is still not exercised by fixtures. Any down, timeout, mismatched
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

## Live read-mostly smoke run

`scripts/staging_smoke_run.py` is the live counterpart: one fenced, GET-only
pass over the deployed staging build and the TWO Staging guild that writes one
[run record](staging-e2e-run-record.md). It composes
`staging_health_contract_probe.py` and `staging_automation_read_smoke.py`, so
the pinned guild, application and origin fences have a single authority.

```sh
STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \
DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \
    python3 scripts/staging_smoke_run.py --tester "QA" \
        --expected-sha <deployed 40-hex SHA> [--deploy-run-id <deploy-staging run id>] \
        [--record staging-smoke-run-record.json]
```

Exit 0 is PASS, 1 is NEEDS WORK, 2 is a fence refusal with nothing sent. Run it
after every `deploy-staging` run you accept; the deploy gate proves the rollout,
this proves the build answers readiness and the guild publishes the surface.

| Row | Passes when | Failure signature |
| --- | --- | --- |
| `GET /health` | 200 with the exact `{"status":"ok"}` shape | `SMOKE-HEALTH-FAIL` |
| `GET /readyz` | 200 and every component ready; a parked or down container fails | `SMOKE-READYZ-NOT-READY`, `SMOKE-READYZ-DB-BEHIND` |
| `readyz build identity` | `build_revision` equals `--expected-sha` | `SMOKE-BUILD-MISMATCH` |
| `identity and command list` | the token is the staging application and the guild command list reads | `SMOKE-DISCORD-REFUSED` |
| `/rank`, `/leaderboard`, `/help` | listed and their own resource is scoped to the staging guild | `SMOKE-REGISTRY-MISSING`, `SMOKE-REGISTRY-DETAIL-MISMATCH` |
| every other built-in | listed (`pass`) or unpublished (`skipped`, gate off or publish pending) | none: a skip is visible in the verdict, not a failure |

The record's deploy run id defaults to the `readyz` `build_id` prefix (the build
id is `RUN_ID-RUN_ATTEMPT` of the `deploy-staging` run that built the image), but
only when the recorded revision is the serving build: that run deployed the
serving revision, so it cannot vouch for a different `--expected-sha`. When the
two differ, pass `--deploy-run-id` for the run that should have deployed the
expected revision. `--deploy-run-id` always overrides the default. With neither
a tested revision nor a usable deploy run id the run prints its results, writes
**no record** and exits 1: a missing revision is NEEDS WORK, not a waiver.

### What it does not prove

- **No slash command is invoked.** Discord creates interactions only for real
  users and the Worker has no HTTP interaction ingress, so no bot token can
  prove a reply. Reply content and timing stay with the offline harness above
  and the human-tester steps in the command matrix.
- Registry presence is not behaviour: a published `/rank` can still fail its
  database read. `GET /readyz` covers the database component, not a query.
- It does not enable a gate. `/attendance` stays `skipped` until
  `TWO_COMMUNITY_SCORECARD` is on in the staging Worker.

### Why it is not a GitHub workflow

The guild reads need the staging bot token, which lives in the staging Worker
and in agent environments, not in a GitHub environment. Adding it there would
distribute a credential, so the run stays a manual, operator/QA step. The
credential-free half (`/health`, `/readyz`, build identity) is already enforced
on every deploy by the `deploy-staging` gate.

Offline coverage: `scripts/test_staging_smoke_run.py` (live-guild refusal before
any request, container down, parked, db-behind and mismatched builds, missing
core surface, foreign or rejected token, token never in output, record schema,
drift against the command matrix).
