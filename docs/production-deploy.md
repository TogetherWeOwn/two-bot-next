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

The `sha guard` job refuses every dispatch until the Environment has
reviewers and a main-only branch policy. `PRODUCTION_AUTO_APPROVE=true`
excuses only the missing-reviewers refusal, and only when the latest
`deploy-staging` run on the SHA is a completed success; the main-only branch
policy stays mandatory with or without it
(see [PRODUCTION_AUTO_APPROVE](#production_auto_approve)). A job that
names a missing Environment makes GitHub create it with no protection, so the
guard checks before the deploy job can run. This repository is public, so
required reviewers work on every plan; no Enterprise plan is needed.

**Deploy.** Dispatch with `sha` set to a full 40-character commit that is on
`main`. That commit needs successful, completed `ci-ok` and `worker check`
runs from GitHub Actions and a successful `deploy-staging` run. `ci-ok` is the
full verdict over lint, worker checks and every selected Rust/DB test lane;
a green lint-only `check` job is not enough. The guard enumerates `check.yml`
runs for the exact SHA without a success filter, selects the newest run number,
and requires its **current attempt** to be completed/success. Both `ci-ok` and
`worker check` must be completed/success jobs in that attempt, with check-run
URLs, SHA, suite and GitHub Actions App identity bound to those jobs. An older
green aggregate cannot authorize a newer queued, running, failed or cancelled
run that has not created its aggregate yet. Missing metadata, incomplete
pagination and a run/attempt change during validation fail closed. If a partial
rerun omits a required job from the current attempt, rerun **all jobs**; do not
reuse the earlier attempt's receipt. The summary records the admitted run/attempt.
Staging runs queue in a single concurrency group, so an intermediate commit may
never stage. Pick one that did. A push to `main` that touches only docs, root
markdown or repository chrome (the `paths-ignore` list in `deploy-staging.yml`)
starts no staging run either, so the newest `main` commit may have no
`deploy-staging` run: pin the latest commit that changes runtime inputs, or
dispatch `deploy-staging` for the head you need. Docs-only commits after a
staged commit change nothing the Worker or container serves.
After the reviewer approves (or the automated approval passes), the job:

1. checks out exactly that commit and re-verifies that it is on `origin/main`;
2. renders the build-identity config and runs
   `wrangler deploy --config <rendered> --env production --message <sha>`;
3. writes the SHA and the old and new Worker version IDs to the run summary;
4. gates on `/health` 200 and on `/readyz` reporting this SHA (see
   [Build identity and the `/readyz` gate](#build-identity-and-the-readyz-gate)).

**Roll back.** This dispatch is the single production rollback method.
Dispatch again with `rollback` set to the previous version ID
from the failed run's summary and `takeover: true`. Set `sha` to the commit that version was built
from. It is recorded as the rollback message, and the `/readyz` gate after the
rollback must report that revision, or a pre-stamp version (see below). The
rollback passes the same guard and the same Environment approval. It then runs
`wrangler rollback <version-id> --message <sha> --yes` and fails unless that
version serves 100% of traffic. Without the takeover the fence stays held,
fenced answers carry no build fields, and the gate fails without being a
rollback signal. When the Rust image itself is the fault,
dispatch in deploy mode with `takeover: true` and the prior good SHA instead; a standalone full
redeploy outside this workflow is superseded as a production rollback path.
Coverage: the rehearsal log in
[cutover-rollback-runbook.md](cutover-rollback-runbook.md#7-staging-rehearsal-log)
is a dry-walk that checked this route without executing a rollback or deploy;
the staging rollback drill
([ci-security.md](ci-security.md#staging-rollback-drill-manual)) rehearses
fence, unforced deployment with immediate Durable Object update, takeover and
restore, which differs from this dispatch's `rollback --yes` with deferred
Durable Object default. The deploy-mode path with a prior good SHA has no
production drill record.

## Build identity and the `/readyz` gate

A deploy stamps its image the way `deploy-staging` does. The rendered Wrangler
config sets `image_vars` on the production container: `BOT_BUILD_REVISION` is
the guarded 40-hex SHA, and `BOT_BUILD_ID` is `<run id>-<run attempt>`. Wrangler
passes both to `docker build` as build arguments, the Rust binary compiles them
in, and `/readyz` reports them as `build_revision` and `build_id`. Neither value
is secret. The SHA comes from the guard, not `GITHUB_SHA`: a dispatch runs on
the head of `main`, which can be newer than the commit being deployed.

Production renders its own config instead of calling `staging_rollout.py
prepare`. That path also snapshots the staging Cloudflare application and
checks the staging ownership receipt, which production does not use. Ownership
takeover runs as the P2–P4 steps in
[Production ownership takeover](#production-ownership-takeover-p2p4) below.
`scripts/production_deploy.py
render` reads the checked-in `wrangler/wrangler.toml`, makes its paths absolute
because the rendered file lives outside `wrangler/`, and sets `image_vars` on the
single production container. Nothing else changes, and
`scripts/test-deploy-production.py` pins that diff. The lockfile's Wrangler
(4.147.0) runs the deploy: `wrangler-action` uses the installed version when
`wranglerVersion` is omitted, and the job runs `npm ci` first.

The gate polls `/readyz` every 10 seconds for up to 30 attempts. In deploy
mode it passes an answer only when its JSON `build_revision` equals the
guarded SHA **and** its `build_id` equals this run's `<run id>-<run
attempt>`: the build ID proves the new container serves, not a previous
build of the same SHA still draining. Rollback mode checks the revision
only, because the serving version was built by an older run.

| `/readyz` answer | Result |
|---|---|
| 200, revision is the SHA, `build_id` is this run's (deploy) or any stamped id (rollback) | Pass: gateway ready |
| 503 with the SHA and a passing build id (same rule) | Pass: gateway parked (truthful 503 as a state, identity still matches) |
| 503 `{"error":"ownership_fenced"}` with no build fields | Keep polling; fail at the end. The fence releases at cutover step 3.5, so the identity match is established only after the takeover |
| Revision is the SHA but `build_id` is another run's (deploy mode) | Keep polling; fail at the end: the previous container still serves this SHA |
| `build_revision` is another SHA | Keep polling; fail at the end |
| `build_revision` and `build_id` are both `unknown` (not stamped) | Deploy: keep polling; fail at the end. Rollback: recorded as a pre-stamp version, not a failure |
| `build_revision` is `unknown` and `build_id` is not | Keep polling; fail at the end |
| `build_revision` or `build_id` is missing or not a string, or the body is not a JSON object | Keep polling; fail at the end |
| Any other status, including `000` (no answer) | Keep polling; fail at the end |

Polling matters because a replaced container can keep answering with the
previous revision — or, on a same-SHA redeploy, the previous build — for a
while. The gate sends an explicit agent,
`two-bot-next-production-rollout/1.0`, the production twin of the staging
gate's agent. The staging gate sets one because the edge rejects Python's
default agent. The run summary records the status, the state and the
`build_id`, so the watch log can tie an answer to one run.

In rollback mode the same gate reads the revision that the rolled-back version
reports, so `sha` must be that version's commit. A version built before this
change reports `unknown` for both fields. The gate records that as a pre-stamp
version and does not fail the rollback. Any other revision fails it.

## Production ownership takeover (P2–P4)

Implements P2–P4 of the approved design
([cutover-production-takeover-design.md](cutover-production-takeover-design.md)).
Staging keeps its own client and behavior; nothing here renames, repoints, or
relaxes the staging gate. This step never activates production: GO stays the
cutover lead's separate decision on the B4 execution card
([cutover-sequence.md](cutover-sequence.md) §§4–5).

**Requesting it.** Dispatch with `takeover: true` (default `false`). The guard
validates the flag and records `Takeover: requested/not requested` in the run
summary. With `false` the job deploys and stops: the fence stays held, fenced
answers carry no build fields, and the gate fails without being a rollback
signal.

**Order inside the `production` job** (same Environment approval as the deploy):

1. `Require production ownership-control configuration`: `preflight` through
   `wrangler/scripts/production-ownership-control.mjs` before anything is
   replaced. It refuses a missing/short control token or a non-production
   origin before the deploy.
2. Deploy (or rollback) runs as before. `Record the Worker version and SHA`
   exports the single Worker version serving 100% as `NEW_VERSION`, and
   refuses the takeover when traffic is split or unreadable.
3. P2 `Read production ownership state without starting`: authenticated GET
   against the production Worker URL. It confirms the serving deployment is
   the recorded `NEW_VERSION` with `running=false`; any mismatch is NO-GO
   and the run stops before any POST. The full response is tee'd to the run
   log as the P2 receipt.
4. P3 `Take over production ownership at the read epoch`: one POST with
   exactly `{"action":"takeover","expectedEpoch":<P2 epoch>,"actor":...}`.
   The actor is `github-actions:<run id>:<guarded SHA>` (audit label only;
   authentication is the bearer token). The explicit release (`true` only in
   these steps) authorizes unparking; a routine deploy never unparks. Only
   5xx POSTs retry inside a bounded window with the epoch re-read first; a
   5xx after a committed takeover is confirmed by GET, never re-posted.
   401, 400, 405 and 409 never retry. Every refused state answers with the
   design's documented code (401/400/405 outside the fence vocabulary, 409
   on epoch conflict, otherwise 503 `ownership_fenced` with `no-store`).
5. P4: the `/health` + `/readyz` gate above runs the first owned probes
   (one container start, revision/build-ID match, single gateway session),
   then `Re-read the active production version after takeover` plus
   `Confirm the taken-over version still serves` prove the taken-over
   version still serves 100%.

**Secrets.** GitHub `production` Environment only — no host-held secrets, no
new secret surface. `PRODUCTION_OWNERSHIP_CONTROL_TOKEN` (Environment
secret, ≥32 chars; provisioning/rotation is a separate governed step, never
part of takeover) is read only inside the `production`-gated job and never
forwarded into the container. `PRODUCTION_WORKER_URL` (Environment variable,
non-secret) must be `https://` and differ from `STAGING_WORKER_URL`; the job
and the client both refuse otherwise, and the client never follows redirects.
Receipts, logs and cards name only bindings, version IDs and fixed refusal
words — never secret values.

**Rollback.** On any failed gate, follow the ordered checklist in
[cutover-rollback-runbook.md §4](cutover-rollback-runbook.md#4-ordered-rollback-steps)
and the full procedure in [cutover.md §Rollback](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy):
fence the singleton first (`fence` action of the same client, with the
current readback epoch and the explicit release), reconcile before
reopening, then revert traffic with a `rollback=<previous version-id>`
dispatch from the watch header. The same guard, Environment approval and
takeover order apply to the revert.

## `PRODUCTION_AUTO_APPROVE`

The repository variable `PRODUCTION_AUTO_APPROVE` is unset by default. Only the
exact value `true` changes the guard. With it set, the guard:

- accepts a `production` Environment with no required reviewers, provided the
  latest `deploy-staging` run on the SHA is a completed success. With no
  reviewers and the variable unset, the guard refuses;
- records `Approval: automated (PRODUCTION_AUTO_APPROVE)` in the run summary.

It does not remove reviewers. If the Environment has required reviewers,
GitHub still pauses the `production` job until one of them approves, and the
variable only adds the staging check.

When the Environment has no reviewers, setting the variable removes the human
approval step from production dispatches. A repository administrator sets it
under Settings, then Secrets and variables, then Actions, then Variables. Who
approves production is a CEO and CISO decision, so enabling the variable needs
their decision first. The guard does not record who set the variable or why.
Read the variable and the Environment's reviewers again before each dispatch.

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

- `readyz`: HTTP status and the four always-present components (`process`, `gateway`,
  `database`, `token_invalid`) (`crates/bot/src/server.rs:228-240`, `:177`, `:191-209`;
  re-checked at `bce86a791`); 200 needs every component `ready`. Only a
  `gateway` at `down` or `starting` is parked; a `database` or `token_invalid`
  at `down` is a fault. 503 parked is truthful, never acceptance; sustained
  503 past the measured recovery budget is a rollback trigger.
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
  `custom_commands_init_failed`, `milestones_load_failed`, `automod_config_invalid`,
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
| `readyz` 503 recovery | First 200 within 60 s of a restart or deploy event | 503 sustained past 60 s post-event: freeze writers, investigate; roll back if no recovery path is identified by the checkpoint | [cutover.md](cutover.md) §48-hour watch; B2's actual outage-start-to-verified-recovery criterion is separate ([staging-soak.md](staging-soak.md)) |
| Restart loop | Zero unplanned restarts; each restart RESUMEs from a checkpoint at most 15 min old (or one armed IDENTIFY on first boot), and any termination exits nonzero so the process cannot sit as a health-200 zombie | Any unplanned restart is a finding; a crash loop (consecutive starts never reaching 200, or repeated supervisor restarts): freeze writers, evaluate rollback | [gateway-recovery.md](gateway-recovery.md) (15-min policy, supervision); [cutover.md](cutover.md) §48-hour watch |
| Session-start (IDENTIFY/RESUME) | One session start per clean restart (RESUME on a checkpoint at most 15 min old; first production boot IDENTIFYs via a one-shot armed directive); the executor records every reconnect, invalid session and the current session-start budget | A restart loop consuming the recovery reserve — repeated fresh IDENTIFYs, an invalid-session storm (opcode 9 `d: false`, close 4007/4009), or a budget reading that no longer allows recovery: investigate; roll back if the gateway cannot hold a session | [Discord gateway session-start limits](https://docs.discord.com/developers/events/gateway#session-start-limit); [gateway-recovery.md](gateway-recovery.md); [cutover.md](cutover.md) §48-hour watch |
| REST 429 | 429s at most 10% of REST requests between alert-rule samples (minimum 10 requests); rolling count of 401/403/429 invalid responses under 5000 per 600 s | Breaker open (rolling count at 5000 per 600 s), or the `rest_429_rate` alert firing across consecutive samples after containment: stop the workload, freeze writers; roll back if the new revision caused it | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [rest-guard.md](rest-guard.md); [runbook.md](runbook.md) Alert: REST 429 |
| REST 5xx and action latency | Single-attempt calls keep their 5 s body-read deadline; 429 `retry-after + 250 ms` honored, 5xx backoff 500…8000 ms across the bounded retry budget; moderation uses one timed attempt | Any uncertain send without a recorded disposition, or a route whose `job_consecutive_failures` reaches 3: freeze that writer and reconcile; roll back if the failing path shipped in this revision | [rest-guard.md](rest-guard.md); [pacing-backoff-acceptance.md](pacing-backoff-acceptance.md); [metrics.md](metrics.md#off-container-scrape-and-alert-rules) |
| Bot token | Zero 401s on bot-authenticated endpoints | First latched `token_invalid`: stop retries, freeze writers; no rollback until provisioning is corrected through the governed path | [rest-guard.md](rest-guard.md) |
| Unban queue | Sweep every 30 s (`UNBAN_SWEEP_INTERVAL_SECONDS`), at most 25 jobs claimed per sweep; no due unban left pending across sweeps without a named disposition | Any overdue sanction without a named disposition is a finding; zero unexplained overdue is required for GO at each checkpoint | [member-moderation.md](member-moderation.md); [cutover.md](cutover.md) §§T-minus, 48-hour watch |
| Scheduled jobs | Last success within twice the job cadence; fewer than 3 consecutive failures | `job_stale` or 3 consecutive failures on a watch-critical job (unban sweep, session checkpoint): freeze the consumer, fix the dependency; roll back if the regression shipped in this revision | [metrics.md](metrics.md#off-container-scrape-and-alert-rules) |
| DB pool | Idle connections above zero, below max | Pool at max with zero idle for 3 consecutive keepalive samples: do not restart to free it; freeze writers, fix the holder; roll back if a new query path holds checkouts | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md) Alert: DB pool |
| `db_errors` | Fewer than 3 storage failures between keepalive samples; a counter reset (process restart) skips the window, not proof of health; sustained low-rate failures surface through `job_consecutive_failures` | 3 or more storage failures between samples: correlate the `op` label and recent deploys; do not run SQL probes or restart to clear errors; escalate repeated bursts per the runbook, evaluate rollback if this revision introduced the failing writes | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md#alert-db-errors) |
| `send_admission_blocked` | Fewer than 3 consecutive keepalive samples with new admission refusals; a sample with no new refusals breaks the streak; admission SQL failures count in `db_errors`, not refusals | New admission refusals in 3 consecutive samples: investigate cooldowns and held lanes; do not replay uncertain sends or restart to free the lane; escalate persistent refusals per the runbook, evaluate rollback if this revision introduced the regression | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md#alert-send-admission-blocked) |
| RSS and placement | RSS near the B1 soak-measured floor (~140 MiB, under the ~200 MiB `lite` gate signal) on the shipped `basic` placement; image/binary sizes inside the B1 ceilings (25% image and 40% binary headroom policy) | Sustained RSS growth versus the B1 floor with no attribution, sustained use pressing the placement cap, or any OOM-kill: freeze writers, investigate or roll back | [b1-baseline.md](b1-baseline.md) (`basic` verdict, ceilings); [cutover.md](cutover.md) §48-hour watch; this is a separate production-watch signal, not B2's numeric RSS acceptance |
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
