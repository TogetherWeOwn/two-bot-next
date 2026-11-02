# Five-minute user-flow synthetic (staging + passive prod)

The `synthetic-user-flow` workflow probes the main HTTP user flow every five
minutes: `GET /health` plus `GET /readyz` with build identity, on staging and
— passively — on production. Both probes are read-only, send no credentials,
follow no redirects, and refuse any origin outside their own allowlist before
a request is sent. No Gatus or third-party monitor is used.

## What runs

| Job | Probe | Target |
| --- | --- | --- |
| `staging` | `scripts/staging_health_contract_probe.py` | `$STAGING_WORKER_URL` (repo var) |
| `prod-passive` | `scripts/prod_passive_probe.py` | `$PRODUCTION_WORKER_URL` (repo var) |

Each job writes one JSON evidence receipt (`synthetic-staging-<run>.json`,
`synthetic-prod-<run>.json`, 14-day artifacts) and fails on any check FAIL
or fence refusal. Production is passive: the prod job observes and alerts
only, and never blocks a deploy. The production promotion reuses the same
probe once as a baseline (see
[production-deploy.md](production-deploy.md#regression-gate-against-production)):
a journey failing on production lowers that baseline, it never blocks a deploy.

### Promotion prerequisite

The scheduled run is not the only check. Every `deploy-staging` run also runs
the staging probe once, as the `user-flow-synthetic` job ("staging user-flow
synthetic (exact SHA)"), right after its deploy and pinned with
`--expected-sha` to the commit that run deployed. `certify-promotion` needs
that job, and the `deploy-production` guard refuses a promotion unless the
job of that exact name succeeded in the same staging run and attempt it
promotes. A failed, skipped or missing synthetic leaves the commit staged but
never promoted. This job has no incident-issue path: the five-minute
scheduled run keeps alerting. Rollbacks and the live-SHA redeploy waive
staging history, so they never read it.

The production probe accepts only a stamped identity with a 40-character,
lowercase hexadecimal `build_revision` and a `build_id` containing exactly two
non-empty decimal components separated by one hyphen (each at most 20 digits;
64 characters total maximum), or the exact unstamped pair
`build_revision: "unknown"` and `build_id: "unknown"`. A mixed pair or any
other shape fails the separate build check with a fixed diagnostic; malformed
remote values are omitted from stdout, stderr and the receipt. This check does
not change the independent `/readyz` health classification. The identity is
observational only: this workflow does not compare it to a deployment SHA and
does not gate or authorize a deploy.

## Failure alerts

Alerts go to the agents who can fix the outage, never to a chat channel.
Two consecutive failed probes open one GitHub issue labelled `incident` per
environment via `scripts/synthetic_alert.py` and the shared sink
`scripts/incident_issue.py`. Once that alert is delivered, further failed
samples are suppressed until a successful probe posts one recovery, which
comments on and closes the issue;
an undelivered alert is instead retried on later failed samples, and if none
was delivered a late ALERT (with the `probe failed` fallback summary, linking
that green run) posts on the first successful sample before any recovery.
This mirrors the Worker's failure-threshold and one-alert-per-incident
behavior. A decide step computes the transition and writes `event` to
`GITHUB_OUTPUT`; the post step runs only when
`!cancelled() && steps.decide.outputs.event != ''`, so a no-op is `skipped`
(never `success`) and a failed probe cannot skip the post via GitHub's
implicit `success()`. Only a successful post counts as delivered in
`scripts/synthetic_alert_history.py`; `skipped` or missing delivery leaves
the incident open, so a history-read no-op can never drop a recovery or fake
a delivery.

Incident state is rebuilt from this workflow's own completed scheduled runs
on `main` (the only state store; no extra permissions or infrastructure).
Reconstruction pages the runs list 15 runs at a time — a page is sized to
the 1 MiB cap on history responses, since one Actions run object measures
15–40 KB and a maximal 100-run page would exceed the cap; an oversized
response reports unknown history instead of being parsed truncated — and
reads jobs newest-first until the collected samples determine the incident
state by themselves — replaying them from any prior state converges,
confirmed by one further sample — or the history runs out. A sample whose
decide step succeeded with no event due (post step skipped) in a normally
finished job is an explicit decided-none record: on a healthy probe a
known-closed checkpoint, because with an open incident a healthy sample
always decides recovery or the late alert so the post step would have run;
on a failed probe a known-open record, because with unknown history it
would have fail-open alerted and with an undelivered incident it would have
re-alerted, so two such records in a row prove the incident is open and
delivered. A cancelled or timed-out job finishes neither way and its post
step is skipped by the `!cancelled()` gate even when an event was due, so
it records neither checkpoint. To keep the record honest, a healthy run
whose history read failed fails its decide step after writing the empty
event (its job shows red even though the probe passed); later
reconstruction treats such a sample as ambiguous, never as closed. (Runs
recorded before that decide-step record existed, during their own failed
history read, are indistinguishable from known-closed ones; that only
matters for an incident open across the change, and the window closes as
those runs age past the bound.) In the healthy steady state the newest
sample is a known-closed checkpoint and mid-outage the two newest samples
are known-open, so either way a decision costs one runs-list read plus a
couple of jobs reads — well inside the `GITHUB_TOKEN` hourly request
budget at the five-minute schedule for both environments; only histories
without decided-none records (for example jobs recorded before the decide
step existed) scan back toward the incident boundary, capped by the bound.
The scan is bounded at 200 prior runs
(about sixteen hours at the five-minute schedule). Delivery evidence on the
oldest retained failure counts, so an outage that outlives any fixed window
keeps its once-per-incident contract: one delivered opening alert, no
duplicate while it stays down, and exactly one recovery on the first healthy
sample (or the late ALERT above when the opening alert never delivered).
When reconstruction cannot be completed — the history read fails, or no
checkpoint is found within the bound — the incident state is unknown rather
than guessed: the documented fail-open policy applies (alert on a real probe
failure, send nothing on a healthy sample), and a fail-open alert that
delivers becomes fresh delivery evidence for later samples. One bound
exception: if all 200 scanned samples are healthy and no post step ever ran,
reconstruction settles closed (the same replay-from-closed as when history
runs out), so a long history outage cannot latch every later decision into
unknown; the accepted cost is that an incident whose evidence rolled past
the bound behind 200 such samples loses its recovery post. Skipped,
cancelled and manually dispatched runs are not probe samples and never move
the state. The manual `force_fail: true` staging proof bypasses the
threshold intentionally; it applies to staging only, and production stays
passive as described above:

```text
two-bot-next ALERT synthetic-user-flow:<staging|prod>: <failing checks>. Runbook: <runbook URL> | build <revision> <build_id> (git <sha>) | logs <github run URL>
```

### Incident issues

`scripts/incident_issue.py` is the one alert sink for this workflow, the
daily staging rollback proof and the daily digest:

| Check | Incident key | Opened by | Closed by |
| --- | --- | --- | --- |
| Staging / prod synthetic | `synthetic-user-flow:<env>` | ALERT | RECOVERY (first healthy sample) |
| Daily staging rollback proof | `staging-rollback-proof` | failed proof or watch | next passing watch |
| Daily digest | `daily-digest` | owed digest not released | the agent who cuts the tag |

The key lives in a hidden body marker (`<!-- incident-key: <key> -->`). An
ALERT while that key's issue is open adds a comment instead of a second
issue; RECOVERY comments, then closes it. If no issue is open (an agent
already closed it), recovery is a no-op. A `force_fail` dispatch is not
scheduled history, so a healthy scheduled sample that finds its key's issue
still open decides RECOVERY and closes it.

The lookup reads every page of open `incident` issues (100 per page, up to
10 pages) and fails closed past that bound rather than miss the keyed issue
and open a duplicate. Two runs that race to open the same key both re-read
after creating; the one that is not the lowest number closes its issue as a
duplicate and comments on the lowest. RECOVERY closes every open issue for
its key, so a duplicate the race left behind closes too. Synthetic history from before this sink (post step
`Alert the ops channel on incident transition`) counts a Discord alert as
undelivered: an outage that only reached Discord re-alerts into an issue on
its next failed probe. A healthy probe whose Discord post succeeded reads as
closed, so a recovered Discord-era incident cannot mute the next outage.

Authentication is the job-scoped `GITHUB_TOKEN` with `issues: write`; no
secret or environment is needed. Requests go only to `api.github.com`;
HTTP 301, 302, 303, 307, and 308 responses are refused and no redirect is
followed. Responses are capped at 1 MiB and duplicate JSON keys are refused.
Errors expose only fixed classifications or HTTP status codes, never the
token or a response body. Runs with no incident transition write nothing.
When an alert or recovery is due, a missing token or a failed write fails
that step, so a silent notification path cannot read as delivered.

Paperclip runs an hourly DevOps routine that lists open `incident` issues
and creates one DevOps task per issue that has none yet, so a failure
reaches an agent and the owner hears only when a decision is needed.

### History admission and decision receipt

History is admitted before sorting, constructing a jobs URL or replaying any
checkpoint. Runs/jobs roots must be objects with object-array collections;
steps must also be an object array. Run IDs and run numbers must be positive
signed-64-bit integers, never booleans, floats or numeric strings. Events and
job/step names must be nonempty strings. Optional job IDs and step numbers use
the same integer rule. IDs and ordering numbers must be unique, including
across run pages; later pages must contain older run numbers. Every row on a
fetched page is validated, including unrelated/current runs and jobs, before
a matching job is read or a checkpoint can stop reconstruction.

When present, counts must be nonnegative signed-64-bit integers consistent
with the page; a jobs count must match the entire single page (at most 100).
A partial jobs page cannot prove a unique target. Missing optional counts,
status and conclusion fields remain readable for older records, but do not
prove delivery or a decided-none checkpoint. Present status must be completed;
conclusions must be null or a supported Actions conclusion (`success`,
`failure`, `neutral`, `cancelled`, `skipped`, `timed_out`, `action_required`,
`stale`, `startup_failure`). Multiple matching target jobs or probe/decide/post
steps are unknown history, even if their values agree. Cancelled/timed-out
posts do not prove delivery; a skipped post proves a checkpoint only with the
existing successful-decide and normally-finished-job conditions above.

Malformed, ambiguous, incomplete or unsupported history raises a safe
`HistoryError` with `code=history_unknown` and
`next_action=inspect_workflow_history`, never raw remote values. Failed current
probes still decide ALERT (exit 0); healthy probes emit no event and fail the
decide step (exit 1), never a false recovery. No current probe sample also
fails (exit 1), without reading history. The 15-run page size, 200-prior-run
replay bound and 1 MiB response cap remain; pagination additionally stops after
15 pages so filtered rows cannot evade that budget. Hitting the page budget
without a checkpoint reports unknown, not an exhausted history.

Optional `--decide-only --json` prints one bounded JSON object to stdout:
`schema_version: 1`, stable `check_id: synthetic-user-flow:<env>`, bounded
string `run_id` (decimal current ID or `unknown`), nullable `event`, boolean
`history_known`, `code` and `next_action`. Known decisions use
`decision_ready` / `none`; unknown history uses the values above; an absent
probe uses `no_probe_sample` / `none` with `history_known` false, since no
history was read. This receipt is a decision, not proof
of delivery or exactly-once posting. `GITHUB_OUTPUT` still receives precisely
`event=alert`, `event=recovery` or `event=`. Without that file, JSON mode moves
the event line to stderr to keep stdout valid JSON. Without `--json`, text
output is unchanged. JSON is refused with posting or dry-run modes (exit 2).
No retry, alert threshold, transport policy or live workflow flag is changed.

Offline verification (mocked history and issue boundaries, no GitHub calls):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_synthetic_alert*.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_incident_issue.py' -v
```

## Operator bindings

- Repository variables `STAGING_WORKER_URL` and `PRODUCTION_WORKER_URL`:
  the staging and production Worker origins. `PRODUCTION_WORKER_URL` is the
  single source for deploy and passive synthetic checks; remove any production
  Environment override of the same name. A missing variable fails that job
  with an error naming the unset variable, so a run with zero probe samples
  never reads green; the decide step also fails closed on any skipped probe.
- No alert secret. The `synthetic-alerts` environment and
  `SYNTHETIC_ALERT_WEBHOOK_URL` are no longer read and can be deleted.

## Proving the path

Dispatch the workflow from `main` with `force_fail: true`: the staging probe
hits an unallowlisted origin and refuses before making a request. That fence
refusal produces no evidence receipt; the upload step tolerates the missing
file, and the `synthetic-user-flow:staging` incident issue opens with unknown
build identity and the run link. The next healthy scheduled run closes it,
and the Paperclip routine creates a DevOps task for it in between. This
proves the staging failure-to-alert path, not production alert delivery. Verify
the production schedule from its own runs, and verify production alert delivery
from an independently evidenced production failure and its matching alert.
