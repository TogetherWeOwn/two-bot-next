# Production Worker deploy

`.github/workflows/deploy-production.yml` is the only path that deploys the
`production` Wrangler environment. It runs only when someone dispatches it, never
on push, PR or schedule. The B4 cutover executor (TOG-9699) dispatches it from
`main`. Agents never do. The cutover gates in [cutover.md](cutover.md) still
apply.

**Before the first dispatch**, a repository admin must set up three things.

1. Create the `production` GitHub Environment with at least one required
   reviewer, and restrict deployment branches to the single custom branch
   `main`. Turn on prevent self-review.
2. Set the `PRODUCTION_WORKER_URL` variable to the production Worker's
   `https://` URL. It must differ from `STAGING_WORKER_URL`.
3. Optionally add production-scoped `CLOUDFLARE_API_TOKEN` /
   `CLOUDFLARE_ACCOUNT_ID` secrets. Environment secrets override the repository
   secrets that staging uses.

Until the Environment exists with reviewers and a main-only branch policy, the
`sha guard` job refuses every dispatch. A job that names a missing Environment
makes GitHub create it with no protection, so the guard checks before the
deploy job can run. This repository is private. GitHub offers required
reviewers on private repositories only on the Enterprise plan; on Free, Pro
and Team plans, the guard keeps refusing.

**Deploy.** Dispatch with `sha` set to a full 40-character commit that is on
`main`. That commit needs green `check` and `worker check` runs (from GitHub
Actions) and a successful `deploy-staging` run. Staging runs queue in a single
concurrency group, so an intermediate commit may never stage. Pick one that did.
After the reviewer approves, the job:

1. checks out exactly that commit and re-verifies that it is on `origin/main`;
2. runs `wrangler deploy --message <sha>`;
3. writes the SHA and the old and new Worker version IDs to the run summary;
4. gates on `/health` 200 and a truthful `/readyz` (200 ready, 503 parked),
   with the same contract as staging.

**Roll back.** Dispatch again with `rollback` set to the previous version ID
from the failed run's summary. Set `sha` to that version's commit, or any other
green `main` commit, which is recorded as the rollback message. The rollback
passes the same guard and the same Environment approval. It then runs
`wrangler rollback <version-id> --yes` and fails unless that version serves
100% of traffic. The `/readyz` gate runs again after the rollback.

## 48-hour watch log (TOG-9699)

The cutover executor copies this template onto the execution card at `T_0`
(first `/readyz` 200 on the production revision) and fills it in through the
watch deadline `T_0 + 48 h`. Watch checkpoints at +15 min, +1 h, +6 h, +24 h
and +48 h follow [cutover.md](cutover.md) §48-hour watch. Poll read-only on a
short cadence (suggested 60 s); record findings, not every healthy poll.

### Watch header (fill once at T_0)

| Field | Value |
|---|---|
| `T_0` (UTC, ISO-8601) | |
| Deployed commit SHA | |
| New Worker version ID (rollback `sha` if re-dispatching) | |
| Previous Worker version ID (**the rollback command's `<version-id>`**) | |
| Watch deadline (`T_0 + 48 h`) | |
| Named coverage | |

The previous version ID comes from the dispatch run's summary, which records
the SHA and the old and new Worker version IDs. Keep it in this header for the
whole watch so the rollback dispatch never has to hunt for it.

### Watch rows (one per finding and per checkpoint)

| UTC timestamp | `T_0` offset | Signal | Observation | Disposition |
|---|---|---|---|---|
| | +15 min checkpoint | `readyz` / `revision` | `/readyz` status + compiled revision/build-ID match | |
| | +1 h checkpoint | `readyz` / `revision` | | |
| | +6 h checkpoint | `readyz` / `revision` | | |
| | +24 h checkpoint | `readyz` / `revision` | | |
| | +48 h checkpoint | `readyz` / `revision` + sign-off | | |
| | | `gateway` | IDENTIFY / RESUME / READY / RESUMED / invalid session / close code + session-start budget | |
| | | `error-class` | one fixed class below, no raw text | |
| | | `rollback-decision` | GO / EXTEND / ROLLBACK + version-ID record | |

- `readyz`: HTTP status and the two wired components (`process`, `gateway`).
  503 parked is truthful, never acceptance; sustained 503 past the measured
  recovery budget is a rollback trigger.
- `revision`: the exact compiled revision/build ID baked into the Rust
  `/readyz` response (mirrors the `GITHUB_SHA` /
  `GITHUB_RUN_ID-GITHUB_RUN_ATTEMPT` provenance in
  [staging-rollout-gate.md](staging-rollout-gate.md)). Any mismatch with the
  deployed SHA fails the checkpoint.
- `gateway`: record each reconnect, invalid session (`d: false` clears durable
  state; close codes 4007/4009 force a fresh IDENTIFY) and the current
  session-start budget, per the gateway signals in [cutover.md](cutover.md)
  §48-hour watch. No restart loop may consume the recovery reserve.
- `error-class`: fixed vocabulary only, from
  [startup-diagnostics.md](startup-diagnostics.md) —
  `listener_bind_failed`, `gateway_override_invalid`,
  `database_connect_failed`, `checkpoint_load_failed`,
  `milestones_load_failed`, `gateway_runtime_failed`,
  `container_service_failed`, `container_lifecycle_failed`,
  `container_unavailable`. Never paste raw messages, URLs, credentials or
  backup paths into the log.
- `rollback-decision`: at each checkpoint record GO, EXTEND (with new
  deadline), or ROLLBACK. A ROLLBACK row must repeat the previous Worker
  version ID from the header — that is the `<version-id>` the rollback
  dispatch needs. At +48 h record sign-off or extend the watch on the
  execution card.

## Deploy-timer mapping (parity §4)

Verified 2026-10-02 against [parity.md](parity.md) §4 "Scheduled jobs &
timers". All shipped schedules match; the one absence is the documented DROP.
No timer is changed by this slice.

| Timer unit | `OnCalendar` as shipped | Parity §4 expectation | Verdict |
|---|---|---|---|
| `deploy/two-bot-next-backup.timer` | `*-*-* 04:17:00` (`RandomizedDelaySec=300`) | nightly DB→S3, daily 04:17 | match |
| `deploy/two-bot-next-guild-config-backup.timer` | `*-*-* 04:31:00 UTC` (`RandomizedDelaySec=10m`) | sealed Discord config snapshot, daily 04:31 UTC | match |
| `deploy/two-bot-next-restore-drill.timer` | `*-*-01 05:30:00` (`RandomizedDelaySec=600`) | monthly; [backup.md](backup.md): 1st, 05:30 | match |
| *(no `*-rules-gate*.timer` shipped)* | absent | `two-bot-rules-gate-timeout.timer` (gate-stuck report, daily 04:43) is **DROP** as runtime — on-demand report post-cutover | intentional absence, not drift |

`RandomizedDelaySec` spreads the actual fire time after the calendar time, so
the watch log must not treat the calendar minute as exact. See
[backup.md](backup.md) for what each timer runs.
