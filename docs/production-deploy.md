# Production Worker deploy

`.github/workflows/deploy-production.yml` is the only path that deploys the
`production` Wrangler environment. It runs only when someone dispatches it, never
on push, PR or schedule. The B4 cutover executor (TOG-9699) dispatches it from
`main`. Agents never do. The cutover gates in [cutover.md](cutover.md) still
apply.

**Before the first dispatch**, a repository admin verifies three things. The
first and the branch policy were verified live on 2026-10-03 (commands under
Verification in the change that wrote this); re-check them before dispatching,
since Environment settings can drift.

1. The `production` GitHub Environment has at least one required reviewer
   with prevent self-review on, and restricts deployment branches to the
   single custom branch `main`. Live state: one required reviewer,
   self-review blocked, custom branch policy `main` only.
2. The `PRODUCTION_WORKER_URL` variable holds the production Worker's
   `https://` URL. It must differ from `STAGING_WORKER_URL`. The deploy job
   refuses when it is unset, not an `https://` URL, or equal to staging.
3. Optionally add production-scoped `CLOUDFLARE_API_TOKEN` /
   `CLOUDFLARE_ACCOUNT_ID` secrets. Environment secrets override the
   repository secrets that staging uses.

Until the Environment has reviewers and a main-only branch policy, the
`sha guard` job refuses every dispatch. A job that names a missing Environment
makes GitHub create it with no protection, so the guard checks before the
deploy job can run. This repository is public, so required reviewers work on
every plan; no Enterprise plan is needed.

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
  `database_connect_failed`, `store_unavailable`,
  `gateway_pool_connect_failed`, `checkpoint_load_failed`,
  `onboarding_gates_invalid`, `onboarding_init_failed`,
  `milestones_load_failed`, `automod_config_invalid`,
  `automod_executor_failed`, `gateway_runtime_failed`,
  `gateway_task_panicked`,
  `container_service_failed`, `container_lifecycle_failed`,
  `container_unavailable`. Never paste raw messages, URLs, credentials or
  backup paths into the log.
- `rollback-decision`: at each checkpoint record GO, EXTEND (with new
  deadline), or ROLLBACK. A ROLLBACK row must repeat the previous Worker
  version ID from the header — that is the `<version-id>` the rollback
  dispatch needs. At +48 h record sign-off or extend the watch on the
  execution card.

### Signal thresholds (budgets and rollback triggers)

Numeric budgets the executor applies to the rows above. Thresholds only: this
table installs no monitor and assumes no `/metrics` endpoint; each row names
its source, and observation uses the paths in [cutover.md](cutover.md)
§48-hour watch (read-only polls, deployment events, moderator observations).

Watch cadence follows [cutover.md](cutover.md): short read-only polls on an
internal endpoint (suggested 60 s), plus deployment events and moderator
observations. The keepalive probes `/readyz`
every `KEEPALIVE_SECONDS` (default 60 s) and raises `container_unready_alert`
after 10 consecutive failed samples (≈10 min); see
[container-readiness.md](container-readiness.md). That alert is a finding, not
acceptance.

| Signal | Budget (stay green) | Rollback-trigger value | Source |
|---|---|---|---|
| `readyz` 503 recovery | First 200 within 60 s of a restart or deploy event | 503 sustained past 60 s post-event: freeze writers, investigate; roll back if no recovery path is identified by the checkpoint | [staging-soak.md](staging-soak.md) acceptance (redeploy gap under 60 s); [cutover.md](cutover.md) §48-hour watch |
| Restart loop | Zero unplanned restarts; each restart RESUMEs from a checkpoint at most 15 min old (or one armed IDENTIFY on first boot), and any termination exits nonzero so the process cannot sit as a health-200 zombie | Any unplanned restart is a finding; a crash loop (consecutive starts never reaching 200, or repeated supervisor restarts): freeze writers, evaluate rollback | [gateway-recovery.md](gateway-recovery.md) (15-min policy, supervision); [cutover.md](cutover.md) §48-hour watch |
| Session-start (IDENTIFY/RESUME) | One session start per clean restart (RESUME on a checkpoint at most 15 min old; first production boot IDENTIFYs via a one-shot armed directive); the executor records every reconnect, invalid session and the current session-start budget | A restart loop consuming the recovery reserve — repeated fresh IDENTIFYs, an invalid-session storm (opcode 9 `d: false`, close 4007/4009), or a budget reading that no longer allows recovery: investigate; roll back if the gateway cannot hold a session | [Discord gateway session-start limits](https://docs.discord.com/developers/events/gateway#session-start-limit); [gateway-recovery.md](gateway-recovery.md); [cutover.md](cutover.md) §48-hour watch |
| REST 429 | 429s at most 10% of REST requests between alert-rule samples (minimum 10 requests); rolling count of 401/403/429 invalid responses under 5000 per 600 s | Breaker open (rolling count at 5000 per 600 s), or the `rest_429_rate` alert firing across consecutive samples after containment: stop the workload, freeze writers; roll back if the new revision caused it | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [rest-guard.md](rest-guard.md); [runbook.md](runbook.md) Alert: REST 429 |
| REST 5xx and action latency | Single-attempt calls keep their 5 s body-read deadline; 429 `retry-after + 250 ms` honored, 5xx backoff 500…8000 ms across the bounded retry budget; moderation uses one timed attempt | Any uncertain send without a recorded disposition, or a route whose `job_consecutive_failures` reaches 3: freeze that writer and reconcile; roll back if the failing path shipped in this revision | [rest-guard.md](rest-guard.md); [pacing-backoff-acceptance.md](pacing-backoff-acceptance.md); [metrics.md](metrics.md#off-container-scrape-and-alert-rules) |
| Bot token | Zero 401s on bot-authenticated endpoints | First latched `token_invalid`: stop retries, freeze writers; no rollback until provisioning is corrected through the governed path | [rest-guard.md](rest-guard.md) |
| Unban queue | Sweep every 30 s (`UNBAN_SWEEP_INTERVAL_SECONDS`), at most 25 jobs claimed per sweep; no due unban left pending across sweeps without a named disposition | Any overdue sanction without a named disposition is a finding; zero unexplained overdue is required for GO at each checkpoint | [member-moderation.md](member-moderation.md); [cutover.md](cutover.md) §§T-minus, 48-hour watch |
| Scheduled jobs | Last success within twice the job cadence; fewer than 3 consecutive failures | `job_stale` or 3 consecutive failures on a watch-critical job (unban sweep, session checkpoint): freeze the consumer, fix the dependency; roll back if the regression shipped in this revision | [metrics.md](metrics.md#off-container-scrape-and-alert-rules) |
| DB pool | Idle connections above zero, below max | Pool at max with zero idle for 3 consecutive keepalive samples: do not restart to free it; freeze writers, fix the holder; roll back if a new query path holds checkouts | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md) Alert: DB pool |
| RSS and placement | RSS near the B1 soak-measured floor (~140 MiB, under the ~200 MiB `lite` gate signal) on the shipped `basic` placement; image/binary sizes inside the B1 ceilings (25% image and 40% binary headroom policy) | Sustained RSS growth versus the B1 floor with no attribution, sustained use pressing the placement cap, or any OOM-kill: freeze writers, investigate or roll back | [b1-baseline.md](b1-baseline.md) (`basic` verdict, ceilings); [staging-soak.md](staging-soak.md) acceptance; [cutover.md](cutover.md) §48-hour watch |
| Event continuity | Zero unexplained gaps or duplicated effects versus independent moderator observations | Any unexplained gap or duplicated execution is a stop condition: freeze writers, evaluate rollback | [cutover.md](cutover.md) §48-hour watch; [staging-soak.md](staging-soak.md) acceptance |
| Shutdown drain | SIGTERM drain completes inside 35 s (`SHUTDOWN_TIMEOUT_SECONDS` default) | `shutdown_deadline_exceeded` (exit 1): the restart reads the last committed checkpoint; repeated misses block GO until investigated | [configuration.md](configuration.md) |

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
