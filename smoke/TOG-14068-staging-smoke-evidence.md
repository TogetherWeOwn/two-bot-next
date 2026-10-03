# TOG-14068 staging-guild manual smoke rehearsal — evidence (pass 1, 2026-10-03)

Staging guild only. Nothing touched production. No error-contract copy asserted
(scope guard: logged as observations only, and none were observable without the
live Discord pass).

## Tested revision

- Serving build (from live `/readyz`): `9b73b33aef69c003471442fc453efbb970824405`
  (`build_id` `37157866778-1`, deploy-staging run
  `https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37157866778`)
- Repo HEAD at check time: `92ab52939d9adec5f5dbb1e0a8f0186bf1b62e03`
  (deploy-staging run 410 `https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37158768596`
  was `in_progress`; the probes below hit the still-serving `9b73b33a` build)
- Staging origin: `https://two-bot-next-staging.5150.workers.dev`
  (from the deploy log; staging guild `1545644954272137297`, live guild
  `326474832151838730` never touched)

## Smoke result table

| # | Check | Result | Evidence |
|---|-------|--------|----------|
| 1 | Staging bot online/present in staging guild | NOT VERIFIED | No Discord connection available to this agent (Discord service: unavailable); live presence needs a Discord-capable pass |
| 2a | `/health` liveness + latency | PASS | HTTP 200 `{"status":"ok"}`, 2.77 s, 2026-10-03 ~22:45Z |
| 2b | `/healthz` + latency | PASS | HTTP 200 `ok`, 0.04 s |
| 2c | `/readyz` gateway ready + ping/health | NEEDS WORK | HTTP 503: `process=ready`, `gateway=starting`, `database=ready`; gateway not connected so presence/ping-via-gateway unproven |
| 3 | List published commands + run read-only ones | NOT RUN | Live registry read needs `DISCORD_STAGING_BOT_TOKEN` (never requested/hunted/substituted); compiled set below is the candidate list only |
| 4 | Screenshots/evidence | PARTIAL | This file + probe transcripts; no Discord client screenshots (see #1) |

## Compiled command inventory (candidate list, NOT a live-registry claim)

Source: `docs/commands.md` (generated, 27 built-ins). Read-only informational
candidates for the live pass: `/leaderboard`, `/rank`, `/rsvp-attendance`,
`/command-list`, `/feed-list`, `/schedule-list`. Note: `/ping` is a voice-room
control (`crates/core/src/voice_rooms.rs`), not a guild slash command, so the
live pass should use HTTP health + gateway latency for the "ping" step.
Everything else in the registry is moderative or write-path — out of scope for
the read-only pass per the card's scope guard.

## Staging health blocker (why this is NEEDS WORK, not PASS)

- Latest completed deploy-staging verify: `rollout_timeout` —
  `instances=active:1,healthy:0,failed:0,starting:0,scheduling:0`
  (job `https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37157866778/job/111305169331`).
- Live `/readyz` agrees: gateway `starting`, never `ready` during this pass.
- Recent deploy-staging history is all failure/cancelled; staging has no known
  green revision newer than this pass. The live Discord steps (presence, ping via
  gateway, published-list runs) must wait for a green staging deploy with
  `/readyz` 200 plus a Discord-capable pass holding the staging token.

## Verdict: NEEDS WORK

Static half done (health endpoints probed, revision pinned, candidate command
list inventoried). Live half outstanding: presence, gateway ping, published
command runs, screenshots — needs (a) staging gateway `ready`, (b) a
Discord-capable pass with the staging bot token.

## Pass 2 re-probe (2026-10-03 ~23:00Z)

- `/health` 200 (0.43 s) — still PASS.
- `/readyz` still 503, still serving build `9b73b33a` (`37157866778-1`):
  deploy run 410 (HEAD `92ab5293`) still `in_progress`/not yet taken over.
- New detail: `/readyz` now reports
  `gateway_failure: {phase: durable_gateway, class: checkpoint_load_failed}` —
  the gateway is crashlooping on checkpoint load (DB-behind-binary shape), not
  merely slow to start. Re-probing will not turn this green; it needs the
  staging repair path.
- Follow-up: child card "Live Discord pass: presence/ping/published-list when
  staging gateway ready", blocked on TOG-12907 (staging on-call repair card).
  This card closes with the NEEDS WORK report; the live pass resumes there.
