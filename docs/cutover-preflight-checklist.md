# Production cutover pre-flight go/no-go checklist

An operator walks this list top to bottom before any cutover step.
It is a **procedure, not approval to execute it** and takes **no
staging or live action**: it only compiles the existing runbooks into
one go/no-go sheet. Any unchecked item is **NO-GO**.

How to use: check each box only with the named receipt in hand, record
the UTC time and the receipt link on the execution card, and stop at the
first NO-GO. Keep member data, dumps and logs in the restricted
evidence location, never in public comments.

## 1. Candidate pinned and CI green on the exact head

- [ ] The candidate is one full 40-character commit on `main`, merged and
  independently reviewed on its exact head, with that head re-verified
  on `origin/main` before dispatch.
- [ ] `ci-ok` (the full verdict over lint, worker checks and all selected
  Rust/DB test lanes), `worker check`, `pr-lint` and `gitleaks` are green on
  that exact head. A green lint-only `check` job is not enough.
  Missing or incomplete evidence is NO-GO; the deployer never waives it.
- [ ] The resulting deployment digest (commit plus old and new Worker
  version IDs from the deploy summary) is pinned on the execution card.

Source: [cutover.md § Preconditions](cutover.md#preconditions-all-must-pass)
and [cutover.md § Roles](cutover.md#roles-safety-and-evidence);
[production-deploy.md](production-deploy.md) (dispatch SHA rule,
green full `ci-ok` verdict and `worker check` plus a successful staging run,
`main`-only branch policy, checkout re-verification, digest in the run
summary).

## 2. Staging deploy healthy for that SHA

- [ ] That exact SHA has a successful staging run; an intermediate commit
  that never staged does not qualify.
- [ ] Fresh operator observation (not the scaffold-era workflow gate
  alone): `/health` 200 and a truthful `/readyz` (200 ready, 503 parked),
  with the compiled revision/build ID matching the deployed SHA.
- [ ] No sustained 503 past the measured recovery budget, no crash loop,
  no unresolved preflight FAIL, and the persisted ownership fence receipt
  is active where required.

Source: [production-deploy.md](production-deploy.md) (staging
run requirement, `/health` and `/readyz` gate, revision match);
[cutover.md § T-minus checklist](cutover.md#t-minus-checklist) (manifest
and sign-offs attached, no racing deploy or migration, read-only
preflight with FAIL as NO-GO); [runbook § Is it alive](runbook.md#is-it-alive)
for the `/health` and `/readyz` reading contract.

## 3. Rollback owner and artifacts named

- [ ] The cutover lead is named and owns go/no-go, timing, watch and
  rollback; the executor, data and moderation leads are named for their
  steps. Host-only steps use one handoff per step with command and
  rollback.
- [ ] The previous Worker version ID from the dispatch run summary is
  recorded in the watch header as the rollback `<version-id>`; the
  rollback dispatch (SHA plus that version ID) passes the same guard and
  the same environment approval, fails unless that version serves all
  traffic, and re-runs the `/readyz` gate.
- [ ] A fence-capable known-good Worker and image pair is retained;
  rolling back to a pre-fence wrapper is not an option.

Source: [cutover.md § Roles](cutover.md#roles-safety-and-evidence) and
[cutover.md § Rollback](cutover.md#rollback-preserve-next-window-writes-before-reopening-legacy)
(maximum accepted loss of zero acknowledged writes, fence before
stopping, reconciled target before any registry restore);
[production-deploy.md](production-deploy.md) rollback steps and its watch
header fields (previous version ID kept for the whole watch).

## 4. Watch queries reachable before T-0

- [ ] The operator holds the watch header blanks ready: `T_0`, deployed
  SHA, new and previous Worker version IDs, watch deadline
  (`T_0 + 48 h`) and named coverage, with checkpoints at +15 min, +1 h,
  +6 h, +24 h and +48 h.
- [ ] The off-container scrape answers: the authenticated metrics route
  returns a scrape, and container plus Worker logs are readable for the
  window. Polls are read-only on a short cadence; only findings are
  recorded.
- [ ] Each of the five watch queries below returns a read-only answer from
  existing sources, with no SQL, probe or restore against staging or
  production databases:
  gateway session starts (`two_bot_gateway_events_total{event="READY"}`,
  `two_bot_gateway_reconnects_total`, `two_bot_gateway_resumes_total`,
  cross-checked against `gateway shard loop started` log lines);
  REST 429 and 5xx share by route
  (`two_bot_rest_requests_total{route,result}`, hot route read from the
  `route` label before acting); handler latency quantiles
  (`two_bot_handler_duration_seconds` buckets, first 24 h versus second
  24 h); unban-queue depth via live job signals plus the
  `moderation_scheduled_unbans` state counts run only against an
  authorized test-container copy or a backup artifact; restart count
  from `listening` lines, `two-bot container started` lines and a
  counter reset between two scrapes. A counter back at zero means the
  process restarted, not that the window was quiet.
- [ ] Watch rows use the fixed error-class vocabulary and record
  GO, EXTEND or ROLLBACK at each checkpoint; a ROLLBACK row repeats the
  previous Worker version ID from the header.

Source: [production-deploy.md](production-deploy.md) 48-hour
watch log (header fields, checkpoint rows, `readyz` and revision,
gateway, error-class and rollback-decision rows);
[cutover.md § 48-hour watch](cutover.md#48-hour-watch) (real configured
monitor, finding-only reporting, named coverage);
[metrics.md](metrics.md) series contract (gateway event counters, REST
route/result labels, handler-latency buckets, job streaks) and
[runbook § Metrics alerts](runbook.md#metrics-alerts) for the
off-container scrape path. Every table query above runs only against an
authorized test-container copy or a backup artifact, never against
staging or production.

## 5. GO / NO-GO record

- [ ] GO requires all of: no overlapping gateway or writers, final data
  and registry checks match, preflight has no FAIL, fresh READY with all
  required components healthy, no unexplained event gap, duplicate effect
  or missed deadline, and all feature sign-offs plus rollback receipts
  present.
- [ ] Any abort trigger is NO-GO and starts rollback or incident handling
  instead: auth, intent or permission failure, unmapped runtime behavior,
  missed moderation action, unexplained data mismatch, duplicate side
  effect, failed internal-action contract, crash loop, session budget
  exhaustion, or no durable rollback watermark.
- [ ] The decision, loss verification, availability gap and next actor and
  action are announced with the private-detail and public-notice split in
  the communication template.

Source: [cutover.md § Registry swap](cutover.md#registry-swap-first-boot-and-gono-go)
(GO list and abort list);
[production-deploy.md](production-deploy.md) 48-hour watch log
rollback-decision rows;
[cutover.md § Communication template](cutover.md#communication-template).
