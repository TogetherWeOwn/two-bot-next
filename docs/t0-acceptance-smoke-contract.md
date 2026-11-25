# T0 acceptance smoke contract

Offline T0 acceptance contract for the production cutover: what `/readyz`,
the gateway, the command surface, and the denial copy must look like before
the GO decision. No staging execution; the staging journeys in the sibling
supply slice execute *against* this contract.

Source revision: `27d22b42` (`origin/main` at authoring, includes PR #425
actionable denial copy and PR #448 fixture alignment). Machine-readable
companion: `t0-acceptance-smoke-contract.json` (same directory). Every
expectation cites the code that produces it.

> **"Post-425" means PR #425** (commit `96e700da`: actionable denied-path
> copy with next steps), **not HTTP 425 Too Early.** No 425 status code
> exists anywhere in this contract.

## 1. `/readyz` shape

- `GET /readyz` returns **200 only when every component is ready**, else
  **503** with the per-component breakdown — never a bare error string
  (`crates/bot/src/server.rs:1-7`).
- Liveness is separate: `GET /health` and `/healthz` always return 200
  `{"status": "ok"}` when the process answers (`crates/bot/src/server.rs:68-70`).
- Statuses serialize lowercase (`ready`, `starting`, `down`); `ready()` is
  all-ready, so `starting` counts as not-ready
  (`crates/core/src/health.rs:10-40`, test at `crates/core/src/health.rs:204-211`).
- Expected T0 200 body: `components` =
  `[["process","ready"],["gateway","ready"],["database","ready"],["token_invalid","ready"]]`,
  no `gateway_failure` field, `jobs` map informational only, plus
  `build_revision`/`build_id` compiled in (`crates/bot/src/server.rs:86-140`;
  `JobStatus` shape `crates/bot/src/jobs.rs:23-31`).
- T0 must compare `build_revision`/`build_id` to the deployed SHA
  (`crates/bot/src/server.rs:110-116`).

## 2. Gateway healthy state

`GatewayState` (`crates/bot/src/gateway.rs:52-82`):

| State | `/readyz` component | Meaning |
|---|---|---|
| `Connected` | `ready` | Shard connected and identified — **the only healthy state** |
| `Armed` | `starting` | Token present, supervisor (re)connecting |
| `Unconfigured` | `down` | Missing prerequisites, shard parked |
| `Draining` | `down` | Reception stopped, effects draining (set on SIGTERM/SIGINT) |

Ready rule (`crates/bot/src/server.rs:158-171`, pinned
`crates/bot/src/server.rs:424-429`): 200 only with gateway `Connected`
**and** database ping true. `Connected` without database is still 503
(`crates/bot/src/server.rs:448-459`).

Two 503s that are truthful, never acceptance:

- `token_invalid: down` forces 503 while `/health` stays 200
  (`crates/bot/src/server.rs:240-279`); the latch sets on a
  bot-authenticated 401 (`crates/discord/src/ratelimit_guard.rs:217-220`).
- `gateway_failure` appears **only after** the gateway task records a
  failure, as exactly `{"phase":"durable_gateway","class":"<token>"}`
  (`crates/bot/src/server.rs:386-421`); the 12 fixed snake_case class
  tokens live in `crates/bot/src/gateway_failure.rs:33-59` and the shape in
  `crates/bot/src/gateway_failure.rs:100-103`. Never error text.
- Failing jobs never change readiness — only the `jobs` map
  (`crates/bot/src/server.rs:282-342`).

## 3. Top command-surface presence

The publish set (`InteractionRouter::publish_set`,
`crates/core/src/router.rs:817-855`) with all gates on is **32 built-ins**,
guild-only (`dm_permission` false throughout), in legacy publish order
(pinned `crates/core/src/router.rs:1412-1440`):

`rank`, `leaderboard`, `help`, `privacy`, `reopen` (core), `attendance` (scorecard), `command`,
`command-remove`, `command-list`, `command-inspect`, `schedule`, `schedule-remove`,
`schedule-list`, `sticky`, `sticky-remove` (automation), `rsvp`,
`rsvp-attendance`, `lfg`, `lfg-close`, `feed-add`, `feed-remove`,
`feed-list`, `feed-inspect` (announcements), `ban`, `tempban`, `kick`,
`timeout`, `warn`, `purge`, `slowmode`, `lockdown`, `unlock` (moderation).

First definition wins, built-in names are reserved, Discord's 100-command
ceiling applies; DB-backed custom commands are dynamic and not listed.

The T0 smoke spot-checks the top five — one per routing family — which route
to their handlers with **no router-owned reply** (success text belongs to
the feature slice; `response_for_slash` is `None`):
`crates/discord/tests/top5_reply_fixtures.rs:166-172` (`TOP_FIVE`),
routing proof `crates/discord/tests/top5_reply_fixtures.rs:196-213`.

| Command | Handler | Gate |
|---|---|---|
| `/rank` | `Rank` | none (open) |
| `/leaderboard` | `Leaderboard` | none (open) |
| `/rsvp` | `Rsvp` | none (open; announcements-off refuses) |
| `/lfg` | `Lfg` | `Manage Events` (`8589934592`) |
| `/ban` | `Moderation(Ban)` | `Ban Members` (`4`) |

Permission bits: `crates/core/src/commands.rs:240-246`. Open-command proof
(`rank`/`leaderboard`/`rsvp` route with zero bits):
`crates/discord/tests/top5_reply_fixtures.rs:216-237`.
Permission-gate proof (`lfg`/`ban` refuse at 0, route with the bit):
`crates/discord/tests/top5_reply_fixtures.rs:278-334`.
Guild fence: foreign/missing guild is `Ignore` except moderation, which
answers `GuildRestricted`:
`crates/discord/tests/top5_reply_fixtures.rs:372-424`.

## 4. Post-425 denial copy

Every refusal is an ephemeral type-4 callback with exact content and
suppressed mentions (`crates/discord/src/interactions.rs:142-156`; wire
proof `crates/discord/tests/top5_reply_fixtures.rs:337-355`; lifecycle
matrix `docs/interaction-replies.md:42-48`). Source of truth:
`crates/core/src/router.rs:86-100` (constants) and
`crates/core/src/router.rs:251-270` (`RouterRefusal::message`).

- Unknown slash names: `UNKNOWN_COMMAND_REPLY` =
  "I don't recognize that command. It may have been removed or renamed —
  pick it again from the / command list."
  (`crates/core/src/router/replies.rs:11`).
- Stale buttons/menus: `EXPIRED_COMPONENT_REPLY` =
  "That button or menu has expired. Run the command again to get a fresh one."
  (`crates/core/src/router/replies.rs:14-15`).
- Feature-off refusals name the admin-only host-setting path, not a Discord
  role: automations / announcements / moderation / scorecard variants in
  §4 of the JSON fixture.
- Permission refusals name the Discord permission and who grants it: `You
  need the Manage Server permission …`, `You need the Manage Events
  permission …`, and per-verb moderation template `You need the {permission}
  permission to use /{command}. Ask a server moderator or admin to grant it.`
  Permission display names (`crates/core/src/moderation.rs:104-112`); the
  template never leaks the internal `moderation.ban` id
  (`crates/core/src/router.rs:260-268`).
- Top-five primary refusals (pinned
  `crates/discord/tests/top5_reply_fixtures.rs:177-193`): `rsvp` →
  announcements-disabled; `lfg` → Manage-Events-required; `ban` → "You need
  the Ban Members permission to use /ban. …".
- Generic handler failure keeps the `ref` correlation id and adds a retry
  hint: `Something went wrong (ref {8 hex digits}). Please try again — if it
  keeps happening, share this reference with a server admin.`
  (`crates/core/src/router/replies.rs:326-328`).

## 5. T0 smoke checklist

The executor copies this table onto the execution card at first `/readyz`
200 on the production revision (`T_0`) and marks each row PASS/NEEDS WORK
with the observed value. Any NO-GO row aborts to the rollback path in
`cutover.md`, never a retry loop.

- [ ] `/health` 200 `{"status": "ok"}` (§1).
- [ ] `/readyz` 200 with the exact §1 body; `gateway_failure` absent;
  `build_revision`/`build_id` match the deployed SHA (§1).
- [ ] Gateway component `ready` (Connected); 503 parked is truthful, never
  acceptance (§2).
- [ ] No `token_invalid: down`; no `gateway_failure` class present (§2).
- [ ] All 28 commands present in publish order, guild-only (§3).
- [ ] Top five route: `rank`/`leaderboard`/`rsvp` open, `lfg` needs Manage
  Events, `ban` needs Ban Members (§3).
- [ ] Unknown name → re-pick copy; stale control → expired-control copy (§4).
- [ ] Disabled-feature denial names the host-setting enable path (§4).
- [ ] Permission denial names the Discord permission + granter, never a
  `moderation.*` id (§4).
- [ ] Generic failure carries `ref` + retry hint (§4).
- [ ] GO only with every row PASS plus the §5 decision record in
  `cutover-preflight-checklist.md`.

## Offline verification

Authoring is offline: no staging execution, no Operator path. Validate the
artifacts without network, database, or Discord:

```sh
python3 -c "import json; json.load(open('docs/t0-acceptance-smoke-contract.json'))"
cargo fmt --all -- --check
```

`cargo clippy` / `cargo test` (including
`crates/discord/tests/top5_reply_fixtures.rs` and
`crates/bot/src/smoke_error_contract_tests.rs`) run in hosted CI on the
exact head; this worker cannot compile `ring`/`rustls` locally.

## Optional operator handoff report

Use the report to summarize saved Help, occupied/empty naming, and passive
gateway observations for a single serving window. It reads local JSON and
prints an advisory summary. Exit 0 means the report was written. Release
decisions continue to use the existing checklist and protected workflow.

In `handoff.json`, copy the intended window from the verified serving and
watch records. Use the compiled `/readyz` stamps for `build_revision`,
`build_id`, and `deploy_id`. In production, the binary retains its original
staging deploy stamp. Use the verified Worker version, image manifest
digest, and Container application ID, plus the UTC bounds of the watch.

```json
{
  "expected": {
    "environment": "staging",
    "build_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "build_id": "900-1",
    "deploy_id": "2026.11.25.1+aaaaaaa",
    "image_digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "worker_version": "11111111-1111-4111-8111-111111111111",
    "application_id": "22222222-2222-4222-8222-222222222222",
    "start": "2026-11-25T02:00:00Z",
    "end": "2026-11-25T02:05:00Z"
  },
  "observations": {}
}
```

An optional `observations.help`, `observations.naming`, or
`observations.gateway` entry carries `window` (the same fields as `expected`)
and `sha256` (64 lowercase hex characters for the exact input file bytes).
Use an entry only when existing evidence independently ties that particular
file to the recorded window. Leave the entry absent when all you have is a
bare probe result, a shared SHA, or the intended window. The report checks
consistency with supplied attribution. Keep the original provenance in the
operator handoff.

```sh
python3 scripts/t0_acceptance_report.py --handoff handoff.json \
  --help-report help-embed-synthetic.json \
  --naming-report staging-voice-naming-evidence.json \
  --gateway-report passive-gateway-summary.json
```

- **Help.** Supply the saved `help_embed_smoke.v3` report. The original
  deploy-called Help bundle and its checker in #2463 can establish the
  staging readback, deletion, and bracketed deployment window. A production
  branding observation needs its own live readback in the authorized window.
- **Naming.** Supply the `offline-naming-observations` output from #2464.
  An absent transition needs a same-room occupied, empty before cleanup,
  and occupied-again readback. Keep any creator-rename HTTP 403 separate
  from these transitions. The supplied fixture describes only its own readbacks.
- **Gateway.** Supply the saved `passive_gateway_evidence.py` summary from
  #2467. Its SHA and counts need the existing observation's verified
  application and single-image window to be attributable. Zero counts mean
  the signal was unobserved. Only a committed READY or RESUMED counts as
  gateway readiness here. Planned boot paths, closes, and failed reconnects
  are counted separately. Endpoint denial needs explicit passive evidence.
  The hosted checkpoint exercise in #2075 covers that path separately.

Omit a file flag for a missing observation. A bare or invalid source remains
`unverified`. Different observation bytes, build stamps, or window bounds
are `mismatched`. An `attributed` row identifies the matching window and
can still show a failed probe or an unobserved signal. The output contains
no combined pass verdict. Rerun the focused synthetic examples for mixed
same-SHA builds and windows, missing observations, and unattributed inputs.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p test_t0_acceptance_report.py -v
```
