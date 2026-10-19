# Cutover freeze drill (staging only)

Cutover-eve rehearsal for the freeze half of the forward cutover: post a
freeze notice in one staging channel, slow it, lock `@everyone` sends,
prove the command surface still answers, then restore everything and
compare the pre/post state. Staging guild only. This document describes
the rehearsed steps; production authorization lives on the cutover card.

Source of behavior: `crates/core/src/channel_moderation.rs`
(`plan_lockdown` moves only the send, thread and reaction lockdown bits,
`plan_unlock` restores the recorded seed verbatim) and `docs/cutover-sequence.md` §1 (freeze and
drain). The harness is `scripts/cutover_freeze_drill.py`; its offline
checks are `scripts/test_cutover_freeze_drill.py`.

## Scope and guards

- Staging guild only. The harness fences the guild id before building any
  request: the live guild id aborts, any other or missing id refuses.
- One drill channel, named by the Operator at run time. Never the guild's
  announcement or rules channel.
- Drill slowmode is 30 seconds (inside the 0–21600 planner bound).
- The drill never kicks, bans, deletes channels, or touches roles.
- Authenticated Discord reads and writes refuse every redirect before a
  follow-up request, even to the same origin. The fixed failure message and
  receipt omit the bot token, redirect Location, headers and response body.
- On any step failure the harness best-effort restores slowmode, the
  overwrite seed and the notice, then reports the failing step. A restore
  mismatch is `NEEDS WORK`, never a silent waiver.

## Steps (each records UTC start and duration)

| # | Step | What proves it |
|---|---|---|
| 1 | Baseline read | Channel slowmode plus the `@everyone` allow/deny seed, hashed |
| 2 | Freeze notice post | Notice message created in the drill channel |
| 3 | Slowmode on | Channel reads back 30 s |
| 4 | Lockdown | `@everyone` deny carries `SEND_MESSAGES`, allow cleared of it |
| 5 | Command surface verify | Guild command list still answers with `ping` present |
| 6 | Slowmode restore | Channel reads back the baseline value |
| 7 | Unlock restore | Seed restored verbatim, or the overwrite deleted when none existed |
| 8 | Notice delete | Notice gone |
| 9 | Restore verify | Pre/post semantic hash equal, unlocked, notice removed |

Target for the full nine: minutes, with per-step timings in the receipt.
The mock (offline) run records 0 ms per step; live timings come from the
staging run.

## Live run (Operator window)

Preconditions: staging deploy healthy for the pinned SHA (`/health` 200,
`/readyz` 200 with the matching revision), drill channel agreed with
moderators, manual `/ping` observer present in the client.

```sh
python3 scripts/cutover_freeze_drill.py --live \
    --channel-id <staging-drill-channel-id> \
    --reason "cutover freeze rehearsal (staging only)" \
    --confirm-staging --evidence <run-receipt.json>
```

During step 5 the observer runs `/ping` in the locked channel and records
the `Pong!` reply time; the harness read proves the registry answers while
the observer proves a locked member still gets a reply. After step 9 the
Operator confirms in the client: channel unlocked, slowmode at baseline,
notice gone.

## Evidence

- Receipt: `cutover-freeze-drill-evidence.json` (run id, UTC, per-step
  timings, pre/post hashes, restored flag). Names, verdicts, timings and
  hashes only — no tokens, no raw member or message ids.
- Verdict comment: `PASS` with the timing table and restore line, or
  `NEEDS WORK` naming the failing step and the follow-up.
