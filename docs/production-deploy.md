# Production Worker deploy

`.github/workflows/deploy-production.yml` is the only path that deploys the
`production` Wrangler environment. It runs on manual dispatch, on a completed
`deploy-staging` run on `main`, and on a daily schedule backstop — never on
push or PR. A completed-but-unsuccessful staging run promotes nothing; a green
one promotes the commit of the latest successful staging deploy — never its
own run SHA, so a re-run of an older staging run cannot downgrade production —
after a 30-minute soak past the staging-green time
(see [Automatic promotion](#automatic-promotion)). The B4 cutover executor
(TOG-9699) dispatches it from `main`. Agents never do. The cutover gates in
[cutover.md](cutover.md) still apply.

**Before the first dispatch**, a repository admin verifies three things. The
first and the branch policy were verified live on 2026-10-03 (commands under
Verification in the change that wrote this); re-check them before dispatching,
since Environment settings can drift.

1. The `production` GitHub Environment has at least one required reviewer
   with prevent self-review on, and restricts deployment branches to the
   single custom branch `main`. Live state checked 2026-10-15: no required
   reviewers, only a branch policy (the owner removed every human approval by
   policy). With `PRODUCTION_AUTO_APPROVE=true` a rollback still runs without
   a reviewer when its target is a verified rollback snapshot (see **Roll
   back**); only the live-SHA cure (`allow_unstaged`) still needs a human
   reviewer.
2. The repository-level `PRODUCTION_WORKER_URL` variable holds the production
   Worker's `https://` URL. It must differ from `STAGING_WORKER_URL`. Keep this
   as the single source for both deploy and passive synthetic checks: remove any
   production-Environment override of the same name. Repository scope is
   required: the `sha guard` job reads it for the live-SHA probe and names no
   Environment, so an Environment-scoped value alone is invisible to the guard
   and every `allow_unstaged` dispatch refuses before probing. The deploy job
   refuses when it is unset, not an `https://` URL, or equal to staging.
3. Optionally add production-scoped `CLOUDFLARE_API_TOKEN` /
   `CLOUDFLARE_ACCOUNT_ID` secrets. Environment secrets override the
   repository secrets that staging uses.

The `sha guard` job refuses every dispatch until the Environment has
reviewers and a main-only branch policy. `PRODUCTION_AUTO_APPROVE=true`
excuses only the missing-reviewers refusal, and only when the latest
`deploy-staging` run on the SHA is a completed success; the main-only branch
policy stays mandatory with or without it
(see [PRODUCTION_AUTO_APPROVE](#production_auto_approve)). A live-SHA
takeover redeploy waives staging history, so it still needs human reviewers
even with auto-approve on. A rollback with auto-approve on and no reviewers
runs only when its `rollback`, `image` and `sha` together equal the complete
provenance receipt (`has_previous`, `provenance_complete`) inside a
`production-rollback-snapshot` artifact at most 14 days old, uploaded by a
deploy-production run on `main` of this repository; anything else refuses
(10-20 13:31Z: the designed rollback was refused for want of a reviewer
while production stayed fenced). A job that
names a missing Environment makes GitHub create it with no protection, so the
guard checks before the deploy job can run. This repository is public, so
required reviewers work on every plan; no Enterprise plan is needed.

**Deploy.** Leave `sha` empty to promote the commit of the latest successful
`deploy-staging` run on `main` (the normal case: merges keep superseding pending
staging runs, so the newest `main` commit is often not staged). The guard then
applies every rule below to that commit and refuses it if its `ci-ok` or
`worker check` has not finished green. To deploy a specific commit, dispatch
with `sha` set to a full 40-character commit (or an unambiguous 7-40 character hex prefix, which the guard resolves to the full commit via the commits API) that is on `main`. That commit needs successful, completed `ci-ok` and `worker check`
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
never stage; the empty-`sha` promotion picks the latest one that did. A push to `main` that touches only docs, root
markdown or repository chrome (the `paths-ignore` list in `deploy-staging.yml`)
starts no staging run either, so the newest `main` commit may have no
`deploy-staging` run: pin the latest commit that changes runtime inputs, or
dispatch `deploy-staging` for the head you need. Docs-only commits after a
staged commit change nothing the Worker or container serves.
After the reviewer approves (or the automated approval passes), the job:

1. checks out exactly that commit and re-verifies that it is on `origin/main`;
2. admits the exact certified staging artifact, preserves its container digest
   in the production image repository, renders production-only bindings and runs
   `wrangler deploy --config <rendered> --env production --message <sha> --no-bundle`;
3. writes the SHA and the old and new Worker version IDs to the run summary;
4. gates on `/health` 200 and on `/readyz` reporting this SHA (see
   [Build identity and the `/readyz` gate](#build-identity-and-the-readyz-gate)).

Before step 2, a schema gate runs the read-only `production-migrate plan` on
the guarded SHA through the no-reviewer `production-migrate-plan` environment
and refuses a new build when either the cutover or store migration chain is
ahead of its production ledger (`pending_before` or `pending_store_before`).
Apply the named pending versions through the manual `production-migrate`
apply dispatch (reviewed plan required), then re-dispatch. A rollback and a
live-SHA takeover redeploy serve no new schema and skip this gate job
(guard-validated modes only; the production job accepts exactly those),
so recovery never waits on it.

For a new build, `production_schema_gate.py` accepts only the existing
`runner_version=1` read-only `mode=plan` report for `migration_target=production`,
the exact canonical lowercase 40-hex requested SHA and `role=two_bot_migrator_ro`,
with strict integer zero apply counts. Returned host/database/branch fields
must match independently verified, normalized environment pins; neither the
manifest nor raw untrimmed variables are a replacement identity oracle. Both
pending arrays must be strictly increasing positive i64 integers. Minimal,
malformed or contradictory evidence refuses before publication. Valid clean
and drift reports are saved atomically; errors publish no new evidence.

The terminal stdout line is JSON with stable `verdict`, `code` and `next_action`:
`schema_clean` exits 0, `schema_drift` exits 1, input/binding/manifest refusals
exit 2, and subprocess/execution/output errors exit 3. The script's existing
recovery-only invocation returns `recovery_waived` (not `clean`) without a DB
read. Connection warnings remain advisory. See the shared
[code/action and evidence contract](runbook.md#schema-gate-evidence-and-terminal-results)
for exact remediation tokens; no raw child stderr, rejected values, exception
text or traceback is relayed. No apply path, target scope or waiver is added.

Before step 2, a role gate also runs the read-only `two-bot db roles verify`
on the same `production-migrate-plan` binding and refuses a new build whose
grants lag the migrations. Migrations create tables but grant nothing: the
2026-10-16 deploy applied migrations 0424–0441 without reapplying the role
plan, the runtime role lacked grants on the new tables (SQLSTATE 42501), and
the gateway crash-looped while the schema gate stayed green. After every
`production-migrate` apply, the operator reapplies the full role plan
(`two-bot db roles plan`) and `verify` must read 0; the gate enforces it.
Rollback and live-SHA redeploy skip this gate job the same way (they
serve no new-table grants), so recovery never waits on it either. Applying
the role plan stays an operator/host action; the gate is read-only.

Operator setup (once): create the non-secret vars
`TWO_BOT_PRODUCTION_HOST`, `TWO_BOT_PRODUCTION_DATABASE` and
`TWO_BOT_PRODUCTION_BRANCH_ID` (on the `production-migrate-plan` environment)
with the pinned production endpoint host, database name and PlanetScale
branch id, and confirm the
`production-migrate-plan` environment exposes the read-only
`TWO_BOT_PRODUCTION_PLAN_DATABASE_URL` secret. Until the pins exist the gate
fails closed on new builds (rollback and live-SHA redeploy still run).

**Roll back.** This dispatch is the single production rollback method. A rollback,
and a redeploy of a prior good commit, must always set `sha` explicitly: an empty
`sha` promotes the latest staged commit, which during an incident is usually the
build that just broke production (the guard refuses a rollback without `sha`).
Dispatch again with `rollback` set to the previous version ID
from the failed run's summary, `image` set to the previous container image
digest from the same summary's rollback-snapshot line, and `takeover: true`
(the default).
Set `sha` to the commit that version was built
from (full SHA or an unambiguous 7-40 character hex prefix; the guard resolves
a prefix and records the full SHA): take all three from that summary line, which the pre-deploy snapshot
(the provenance receipt) fills in. Use that line only when it names all
three (`rollback`, `image` and `sha`): a line without `sha` is not a
complete receipt, and a manual rollback must not be dispatched from it.
The three inputs are validated as one
receipt before anything is restored: the rollback script refuses unless the
Worker version's stamped deploy message (`wrangler deploy --message <sha>`)
equals `sha` (the resolved full SHA), the version appears in the 100%-serving deployment history,
and the image comes from a completed rollout, or a replaced one whose
record shows every step completed and every instance updated (a rollout
can be replaced before it finishes, so status alone never proves the
image served). A merely CI-green SHA paired
with the wrong version therefore refuses before traffic moves instead of
failing the gate after the restore; do not widen the guard to admit one.
The `/readyz` gate after the
rollback must report that revision, or a pre-stamp version on 200 with
gateway ready (see below). A version with no stamped SHA — deployed before
SHA stamping began, or created by a secret change without `--message` —
cannot form a receipt and a manual rollback to it refuses; the
in-run auto-rollback to such a snapshot keeps its legacy version-header
identity and records `provenance: legacy-no-sha` in its evidence. Keep
every serving version stamped: a secret change publishes a new Worker
version too, so stage secrets with `wrangler versions secret put --env <env> <KEY>
--message <sha>` — where `<sha>` is the SHA of the last successful deploy
(the run summary's `SHA:` line, or the live `/readyz` `build_revision`),
not the pre-deploy snapshot's `serving_sha` (that snapshot describes the
version being replaced, so stamping with it would label the new version with
the previous build's SHA and trip the post-restore gate) — then deploy that
staged version with `wrangler versions deploy --env <env>`, on staging
and production alike. Precheck first: `versions secret put` builds on the
newest-created version (`PATCH .../versions/latest`), not necessarily the
serving one. Confirm the last block of `wrangler versions list --env <env>`
(the list prints oldest first, so the last block is the newest version)
is the serving version (same id as `wrangler deployments status --env <env>`);
the stamp is the SHA that serving version was built from (its
`workers/message`, or the live `/readyz` `build_revision`). If they differ
(after an auto-rollback, a manual rollback, or a `versions upload`), do not
stage secrets — redeploy the intended SHA through the workflow first, then
stage. A secret-change version created by `wrangler secret
put` or by a `versions secret put` without `--message` carries no stamped
SHA and can never form a receipt. If the pre-deploy snapshot records
`serving_sha: None`, the serving version is unstamped: stamp it (secret
change with `--message` as above) before the deploy the snapshot precedes,
or the next manual rollback to it will refuse. The
rollback passes the same guard and the same Environment approval, except the
guard never needs deploy-staging history for a rollback: aged-out staging
runs must not block recovery. It needs no human reviewer when
`PRODUCTION_AUTO_APPROVE=true` and the three inputs match a verified
rollback snapshot (above) — the summary line names all three, so a rollback
dispatched from it always matches; the guard records the snapshot's run in
its summary. The step restores BOTH the named Worker version
and the named container image (a Worker-only rollback leaves the new image
serving), hands ownership to the restored version, and requires the same
stable gateway-ready gate. Without the takeover the fence stays held,
fenced answers carry no build fields, and the gate fails without being a
rollback signal. A standalone `wrangler rollback` outside this workflow is
superseded as a production rollback path: it restores the Worker version but
not the container image.

**Stale bot, live SHA.** When the bot is stale but the live build is good
(the owner's first-response cure), re-serve it without staging history:
dispatch in deploy mode with `sha` set to the currently live commit,
`takeover: true` (the default), and `allow_unstaged: true`. The guard still requires the
SHA on main with green `ci-ok`/`worker check` and the operator's confirmation
that the SHA is live; it waives only the staging-history check. Like a
rollback, it still needs a human Environment reviewer and is refused without
one even with auto-approve on. Anything else
(a new build) always needs its staging run.
Coverage: the rehearsal log in
[cutover-rollback-runbook.md](cutover-rollback-runbook.md#7-staging-rehearsal-log)
is a dry-walk that checked this route without executing a rollback or deploy;
the staging rollback drill
([ci-security.md](ci-security.md#staging-rollback-drill-manual)) rehearses
fence, unforced deployment with immediate Durable Object update, takeover and
restore, which matches this dispatch's rollback immediate Durable Object
update. The deploy-mode path with a prior good SHA has no
production drill record.

## Automatic promotion

A green staging commit reaches production with nobody dispatching it. The
workflow starts on every completed `deploy-staging` run and every completed
`check` run on `main`, and once daily as a backstop (a missed trigger, an
expired hold, an incident pause). Staging and CI finish in either order on the
same commit (on 10-20 staging went green ~23 minutes before CI, and 12 promotions
in a row refused), so whichever finishes last promotes and the earlier one skips.
An automatic run is exactly the empty-`sha` manual dispatch — the commit of
the latest successful staging deploy — with takeover on, so the owned probes
can certify it. Every guard still applies to it, including the exact-SHA staging
user-flow synthetic: the guard refuses unless the `staging user-flow
synthetic (exact SHA)` job succeeded in the staging run and attempt it
promotes (see [the synthetic runbook](synthetic-user-flow.md#promotion-prerequisite)).
These skips keep it green while deploying nothing:

- the triggering staging run did not succeed (`staging-not-green`);
- the triggering `check` run did not succeed (`ci-not-green`) or was not a
  push to `main` (`ci-not-push`);
- CI finished on a commit that is not the latest successful staging deploy
  (`staging-pending`): the staging completion promotes it;
- `check.yml` is still running on the target (`ci-pending`): its completion
  promotes it (a completed red CI still refuses);
- the hold variable is still in the future (see below);
- the target is already live: production `/readyz` already reports the SHA;
- the live build already contains the target (`older-than-live`): an older
  build never replaces a newer one automatically, so a re-run of an older
  staging run cannot downgrade production (intentional downgrades stay an
  explicit operator rollback, which also serves the older container image).

Otherwise the `soak` job sleeps the remainder of 30 minutes past the
staging-green time, recomputed there from the staging-green timestamp the
guard reports (an unreadable timestamp soaks the full window). The soak runs
outside the `deploy-production` lane — only the `production` and `release`
jobs hold it — so a sleeping promotion never queues an operator rollback
behind it. The lane keeps the default single pending slot, so a `recheck`
job waits for the soak and both gates, then re-reads the hold, the live
revision and the active dispatches immediately before the deploy: a hold set,
a target gone live or stale, or an operator dispatch queued, waiting or in
progress while the gates built or the soak slept skips green instead of
deploying through the incident — or entering the lane behind a recovery run
and cancelling it. A failed soak fails closed into no deploy, an unreadable
dispatch list skips green (the backstop retries), and a cancelled run never
deploys. The residual race is a dispatch queued in the seconds between the
recheck and the lane entry: watch the lane when recovering. The run then
deploys through the same schema gate, roles gate, readyz gate and error-rate
gate as a manual dispatch. Automatic runs need either
`PRODUCTION_AUTO_APPROVE=true` or Environment reviewers (which pause the run
for approval); without either the guard refuses.

Hold: the repository variable `PRODUCTION_AUTO_PROMOTE_HOLD_UNTIL` holds
automatic promotion until an ISO-8601 UTC expiry (empty means no hold).
Manual dispatches ignore it. An unparseable hold refuses the run instead of
promoting through it. Setting or clearing the hold is an operator action;
like `PRODUCTION_AUTO_APPROVE`, who set it and why needs a CEO and CISO
decision first. After a manual rollback, set the hold until the incident is
resolved: otherwise the next automatic run re-selects the staged build that
was just rolled back and redeploys it (the live check only skips builds the
live revision already contains, and a rolled-back target is newer than live).

## Rollback-target preflight

Every ordinary promotion proves its rollback target before anything in
production changes. Right after the pre-deploy snapshot, the `production` job
runs `scripts/rollback_target_preflight.py`, which refuses the promotion
(the run fails red, nothing is deployed) unless:

- the snapshot names a complete receipt: a previous serving Worker version,
  the Git SHA it was deployed from and its container image digest (a first
  deploy, an unstamped version or split traffic has none);
- live Cloudflare state still binds that receipt, with reads only: the same
  check the rollback runs before any restore;
- the registry still holds the image digest (`docker manifest inspect` with
  a 15-minute pull-only credential);
- no migration added since the serving SHA is backward-incompatible, and the
  serving SHA is an ancestor of the candidate (see
  [migrations.md](migrations.md#rollback-compatibility-at-promotion)).

Its receipt (version, image, SHA, the migrations it checked, refusal codes)
goes to the log and the run summary; it never carries SQL text or API
bodies. A rollback and the live-SHA redeploy skip it: recovery never waits on
it. The daily `staging-rollback-drill` keeps exercising the restore path
itself on staging.

## Regression gate against production

Before any production mutation, every ordinary promotion compares the
soaked candidate on staging with production as it serves now:

- The `soak` job first records a soak-baseline `/ops/metrics` scrape
  (`wrangler/scripts/rest-5xx-counters.mjs`, same `staging` Environment
  bearer), so the judged soak window starts at that sample, then sleeps out
  the remainder of the 30 minutes past the staging-green time.
- The `staging-soak-evidence` job runs after the soak (outside the lane,
  `staging` Environment with `deployment: false` for the scrape bearer). It
  runs the staging user-flow probe pinned to the guarded SHA and bound to
  the guarded run-attempt build (a same-SHA redeploy no longer proves the
  candidate soaked), one `/ops/metrics` scrape, and a read-only listing of
  the main-branch `deploy-staging` runs. If staging now serves another
  build, an automatic run skips green when a staging run still promotes
  that build (`staging-superseded`: that build promotes itself) and refuses
  when nothing promotes it (the promotion stalled: the serving build has no
  newer green or in-flight staging run behind it); a manual run refuses. The
  scrape receipts carry the process identity
  (`two_bot_process_incarnation`): when the identity changed between the
  baseline and the post-soak sample, the process restarted mid-soak and the
  cumulative counters after the restart no longer cover the soak, so the
  ratio records `insufficient_data` instead of a pass. No baseline is
  expected when the soak skipped (it was already over before the run
  started).
- The `production` job then runs the passive production probe and the same
  scrape against production as the baseline (production's counters since its
  process started, normally a day or more of traffic), and
  `scripts/promotion_regression_gate.py compare` refuses when a journey
  (`health`, `readyz`, `build`) passes on production but failed on staging,
  or when staging's REST 5xx ratio exceeds max(1.5 × production's ratio, 5%)
  with at least 3 staging 5xx responses. Fewer than 10 judged staging
  requests (or no bearer) records `insufficient_data` with a warning, never a
  pass; a production baseline below 10 requests falls back to the 5% floor.

A failing production journey lowers the baseline, never the bar: the gate
only asks that the candidate is no worse than what serves now. The
`transport` result stays out of the 5xx count, as in `rest_5xx_rate`. Rollback
and the live-SHA redeploy skip both legs.

## Post-deploy watch

The in-run gates judge a few minutes. After an ordinary takeover promotion
passes them, the last steps of the `production` job keep watching production
for 30 minutes (`scripts/post_deploy_watch.py`):

- every 10 s: `/health` 200 and `/readyz` 200 with gateway ready and the
  promoted build revision. 18 consecutive misses (~3 min, the deploy gate's
  stability window) is a breach; a single blip is not;
- back-to-back REST 5xx windows through the same
  `promotion-error-gate.mjs` verdict as the deploy gate. The last window
  starts late with a shorter sample and may finish up to 2 minutes past the
  deadline, so the final minutes are judged too. A failing window is a
  breach; two unscrapeable windows in a row are a breach; a missing bearer
  or a quiet window never is;
- every minute, and again right before reporting a breach: the promoted
  Worker version still serves 100%. If not (an operator rollback or a
  manual Cloudflare change moved traffic), the watch stops without touching
  anything.

On a breach the job restores the pre-deploy snapshot (Worker version AND
container image, with takeover) through the same reviewed
`worker_container_rollback.py` as the in-run auto-rollback, and the run fails
red. If the watch cannot read Cloudflare it fails red without rolling back.
Because the watch runs inside the `production` job, the `deploy-production`
lane stays held from the deploy to the end of the watch: no newer deploy
lands mid-watch and no separate job re-enters the single pending slot. An
operator who needs the lane sooner cancels the run. The GitHub Deployment
record is written before the watch; a watch rollback shows in the run
summary and its evidence artifact. As after a manual rollback, set
`PRODUCTION_AUTO_PROMOTE_HOLD_UNTIL` after a watch rollback, or the next
automatic run re-promotes the same build.

## Production error-rate gate

The `production` job ends with the same REST 5xx promotion gate staging
runs (`wrangler/scripts/promotion-error-gate.mjs`, `--user-agent
two-bot-next-production-rollout/1.0`): it scrapes live production
`/ops/metrics` and fails the stage on a `rest_5xx_rate` firing, so the
auto-rollback below restores the pre-deploy snapshot (Worker version AND
container image). Live enforcement needs the `PRODUCTION_METRICS_SCRAPE_TOKEN`
secret (a `production` environment secret, 32+ chars); without it the step
records a skip, never a pass. The gate does not run in rollback mode (after
a restore there is nothing to roll back to). **Failure injection never runs
against production**: the production workflow has no drill/fault input, guard
binding or rehearsal flag. The workflow-policy suite pins the exact six
production inputs and greps for the retired switch so it cannot return, even
as an unarmed default. Recovery proofs run on staging only (see below).
On 10-20 20:54Z a forced production drill deployed, failed the ownership read,
and its auto-rollback refused on image provenance, leaving production degraded
until the next automatic deploy. Never repeat that drill on production.

## Build identity and the `/readyz` gate

Ordinary promotion does **not** rebuild a SHA. Staging bundles the Worker once,
serves those saved modules with `--no-bundle`, and builds the stamped container
once. After deploy/readiness, voice synthetic and the pinned-history guard all
pass, `certify-promotion` uploads `bot-promotion-<run id>-<attempt>`. Fault drills,
rollback proofs and an opted-out voice gate never certify. The diagnostic
`staging-rollout-evidence` upload stays independent and still runs on failure.
Packaging unsets `WRANGLER_OUTPUT_FILE_PATH`: Wrangler's dry run writes session
and deploy records too, and must not contaminate the real deploy's single receipt.
Only **Re-run all jobs** is supported for certification. A partial job rerun can
reuse an earlier deploy attempt; certification intentionally refuses that old
build evidence rather than relabeling it as the new current attempt.

The certificate contains only a closed version-1 receipt and `modules/`: source
repository/SHA/run/attempt, original compiled stamps, verified staging Worker and
rollout identities, image digest, compatibility settings and each module's path,
size and SHA-256. No generated environment config, sessions, diagnostics or
secrets enter it. Intake refuses duplicate JSON keys, unsafe paths/symlinks,
unknown fields/types and extra/missing or mismatched modules. Limits: 64 KiB
receipt, 32 modules, 8 MiB per member and 16 MiB expanded module bytes.

Production independently verifies the current successful main staging run and
attempt, exact workflow/repository and immutable artifact ID/archive digest. It
rechecks the source attempt immediately before copying. Missing, expired,
ambiguous or incompatible artifacts fail **before any production mutation**;
there is no rebuild fallback. Docker pulls/tags/pushes the admitted digest into
the existing production image repository using Wrangler's normal short-lived
registry login, then verifies both RepoDigests and the remote manifest digest.
A changed digest or foreign account/repository publishes no admission. Rollback
image provenance remains production-local; staging image references are not
accepted as production rollback receipts.

`production_deploy.py render --artifact` combines the admitted modules and
production-local digest with the checked-out **production** TOML/bindings only.
It removes image build inputs, disables rebundling and refuses compatibility
drift. Production has its own Worker UUID and Deployment event ID, while the
binary keeps the exact original staging `build_revision`, `build_id` and
`deploy_id`. The readiness gate judges those admitted stamps plus the new exact
serving Worker headers. Before the stable readiness window, a GET-only identity
preflight checks the intended production Worker, application image and target
rollout without demanding running instances: takeover stops the process, and the
first owned `/health` probe starts it. After the window, a separate GET-only proof
requires image convergence and live exact-version `/health` plus original-stamp
`/readyz`. The two supported completed Durable Object counter shapes (active/healthy
0/1 or 1/0) need two consecutive fully passing observations, finished rollout steps
and all instances updated; any miss resets confirmations. Empty rollout listings
or the exact older rollout identified by the pre-deploy snapshot may poll; a newer
foreign-image rollout refuses. A different same-SHA build still refuses. The
production event ID is not restamped into the image.

Rollback and explicitly authorized live-SHA recovery remain independent of
artifact retention. Only live-SHA recovery builds a fresh image. Its rendered
config sets `image_vars` on the production container: `BOT_BUILD_REVISION` is
the guarded 40-character lowercase hexadecimal SHA, `BOT_BUILD_ID` is
`<run id>-<run attempt>`: exactly two non-empty decimal components, each at most
20 digits and 64 characters total, and `BOT_DEPLOY_ID` is the deploy ID
`YYYY.MM.DD.N+sha7` (see below). Wrangler passes all three to `docker build` as
build arguments, the Rust binary compiles them in, and `/readyz` reports them as
`build_revision`, `build_id` and `deploy_id`. No value is secret. The SHA comes from the
guard, not `GITHUB_SHA`: a dispatch runs on the head of `main`, which can be
newer than the commit being deployed.

Production renders its own config instead of calling `staging_rollout.py
prepare`. That path also snapshots the staging Cloudflare application and
checks the staging ownership receipt, which production does not use. Ownership
takeover runs as the P2–P4 steps in
[Production ownership takeover](#production-ownership-takeover-p2p4) below.
`scripts/production_deploy.py
render` reads the checked-in `wrangler/wrangler.toml`. With `--artifact` it
verifies the saved module inventory and renders immutable inputs as described
above. The legacy recovery invocation makes source/build paths absolute and
sets `image_vars` on the single production container; existing protected tests
still pin that legacy diff. The lockfile's Wrangler
(4.152.0) runs the deploy: `wrangler-action` uses the installed version when
`wranglerVersion` is omitted, and the job runs `npm ci` first.

The gate polls `/readyz` every 10 seconds for up to 30 attempts. In ordinary
deploy mode the expected build/deploy IDs are the original admitted staging
stamps, not the new production event's IDs. For live-SHA recovery only, it passes
an answer when its JSON `build_revision` equals the guarded SHA, its `build_id`
equals this run's `<run id>-<run attempt>`, **and** its `deploy_id` equals this
run's deploy ID: the build ID and deploy ID prove the intended container serves,
not a previous build of the same SHA still draining. Rollback mode checks the revision
only, because the serving version was built by an older run.

| `/readyz` answer | Result |
|---|---|
| 200 with gateway `ready`, revision is the SHA, `build_id` and `deploy_id` match the admitted artifact (ordinary deploy), this run (live-SHA recovery), or any stamped values (rollback), held for 18 consecutive polls (~3 min) | Pass: gateway ready and stable |
| 503 with the SHA and passing build/deploy ids (same rule) | Keep polling, then fail (auto-rollback only after a takeover; see below): a parked gateway never certifies a build |
| 200 with gateway `starting` or `down` | Keep polling, then fail (auto-rollback only after a takeover; see below) |
| 503 `{"error":"ownership_fenced"}` with no build fields | Keep polling; fail at the end. The fence releases at cutover step 3.5, so the identity match is established only after the takeover |
| Revision is the SHA but `build_id` or `deploy_id` differs from the precisely admitted artifact (or this recovery run) | Keep polling; fail at the end: the previous container still serves this SHA |
| `build_revision` is another SHA | Keep polling; fail at the end |
| `build_revision` and `build_id` are both `unknown` (not stamped) | Deploy: keep polling; fail at the end. Rollback: recorded as a pre-stamp version, not a failure, but only on 200 with gateway `ready`; otherwise keep polling, then fail |
| `build_revision` is `unknown` and `build_id` is not | Keep polling; fail at the end |
| `build_revision` or `build_id` is missing or not a string, `deploy_id` is missing in deploy mode, or the body is not a JSON object | Keep polling; fail at the end |
| Body exceeds 65,536 bytes, JSON repeats an object key, nests past the depth bound, or `build_revision`/`build_id`/`deploy_id` has an unexpected shape | Keep polling; fail at the end with a fixed single-line reason that echoes no remote value |
| Any other status, including `000` (no answer) | Keep polling; fail at the end |

A single 503 (gateway `starting`) poll never passes the gate — the 10-15
deploy passed on one such poll ~10 s after rollout while already
crash-looping (see the detection gap in
[production-incident-2026-10-15-postmortem.md](production-incident-2026-10-15-postmortem.md)).
Green requires the follow-up ready observation in the first table row: the
200-ready answer held for 18 consecutive polls (~3 min). A nonpassing answer
resets the stability streak, but the gate keeps polling and can still pass on
a later 18-consecutive-poll 200-ready window within the 60-poll stability
budget (~10 min); if no such window completes, the gate fails. After a takeover, a failed gate rolls back
automatically when the deploy and pre-deploy snapshot succeeded; without a
takeover or snapshot the run just fails (see below).

A failed gate after a takeover rolls back automatically
(`scripts/worker_container_rollback.py`): the `production` job's `snapshot`
step records the serving state **before** `deploy`. Its `auto_rollback` step
runs on `failure()` only when deploy mode, takeover, successful deploy and
successful snapshot all hold. It restores that local receipt's Worker version
AND container image with takeover, requires the stable gate on the restored
build and uploads `production-auto-rollback-evidence`. This is in-job recovery:
**no dispatch, new job or reviewer is needed**. The original gate has no
`continue-on-error`, so the run stays red after a successful restore. The
rollback restores service; it never certifies the failed build. The workflow
policy pins this ordering, condition, local snapshot command and red outcome.
Without a takeover or a successful deploy/snapshot the run fails for the
operator; a failed restore is never a reason to wait for the next deployment.

### Daily staging proof

`staging-rollback-drill.yml` runs daily at **03:17 UTC** on `main`. With no manual
`target_version`, its `proof` job calls `scripts/rollback_proof.py run`, which:

1. Dispatches only `deploy-staging.yml` on `main`, with the existing staging
   error-rate gate rehearsal enabled. No production dispatch or traffic fault
   is involved. The GitHub REST `2026-03-10` dispatch response provides the exact
   child run ID; a missing ID refuses without redispatch or guessing "latest".
2. Waits at most 70 minutes for that run. The snapshot, deploy and rollout
   readiness steps must pass, the injected error-rate gate must fail, and the
   in-job auto-rollback and evidence upload must succeed. The child **must remain
   red**; a green child does not prove recovery.
3. Checks `staging-rollout-evidence`: the restore must name the exact pre-deploy
   Worker version and image digest, with takeover and the stable 200/gateway-ready
   gate. It uploads a sanitized `staging-rollback-proof` artifact (`proof.json`)
   containing the child run ID/SHA and restored members, retained for 14 days.
   The wrapper is green only after that proof is validated and uploaded.

The child is named `staging rollback proof`, not a staging certification run.
Production excludes it from latest-certification history only after re-reading
its same-repository main dispatch, exact first attempt, and all six expected
step outcomes above. A name alone, a failed/missing restore, or a running proof
never excuses a red staging run; a proof never certifies a commit for promotion.

The child honors `STAGING_SOAK_HOLD_UNTIL` before the schema gates or deploy:
an armed/invalid hold fails the proof without changing the observed staging
build. Inspect the hold and resume the proof after the observation window;
do not clear a hold to make a proof green. Explicit arm/disarm dispatches without
the error-rate switch retain their exception.

The wrapper uses separate `staging-rollback-proof-schedule` and
`staging-rollback-proof-workflow_dispatch` groups, **not** `deploy-staging`:
a later manual dispatch cannot replace the pending scheduled wrapper, and its
child can acquire the staging deploy lane. The optional manual Worker-only
drill still holds `deploy-staging` while it runs and now uploads its separate
evidence artifact; it does not count as the daily automatic proof.

A failed wrapper opens (or comments on) the one `staging-rollback-proof`
GitHub issue labelled `incident`; Paperclip turns it into an agent task. Independently, the **06:23 UTC** `daily-digest` workflow's
`rollback-proof-watch` job checks the newest scheduled drill and its artifact,
even if the digest fails. Missing, failed, cancelled, stuck, unreadable or
older-than-30-hour proof alerts and fails the watcher. An older successful proof
cannot hide a newer failed one. A healthy check comments on and closes any open issue
with no activity since the graded proof completed. Every failed proof, scheduled
or manual, alerts on that issue, so later activity holds it open until a newer
scheduled proof passes. This independent
schedule catches a missed drill trigger; an outage of GitHub Actions itself is
outside these two scheduled checks.

For a reviewed on-demand proof after a repair:

```sh
# No network, dispatch or mutation:
python3 scripts/rollback_proof.py run --dry-run
# Staging only; use the merged main workflow, never a production drill:
gh workflow run staging-rollback-drill.yml --ref main
```

Inspect the exact child run and staging state before retrying a failed/timed-out
proof. The dispatcher never cancels or blindly reruns a mutating child. A manual
proof is useful evidence, but the freshness watcher requires a scheduled proof.

Both gate curls enforce `--max-filesize 65536` (64 KiB). Any transfer
failure counts as `000`, even if curl reports HTTP 200 and leaves a valid
JSON prefix. The readiness judge independently reads at most 65,537 bytes
and refuses over-cap evidence with `/readyz answered over the body cap`
before UTF-8 decoding or JSON parsing; exact-cap valid evidence is still
graded normally. This byte limit matches the staging probe's body cap.

Remote identity shapes are validated before any value enters diagnostics:
only `unknown`, a 40-character hex revision, a bounded digits-digits
build ID, or a canonical deploy ID can appear in output. Newline, pipe, control, overlong and
secret-shaped values yield the fixed unexpected-shape refusal above. The
raw `/health` and `/readyz` bodies reach the log only through
`production_deploy.py excerpt`, which prints the first 2048 body bytes as
one escaped line (escaping can make that line longer), so a hostile answer
cannot forge log or summary lines.

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

### Deploy IDs, `/version` and Deployment records

Every staging and production deploy carries a deploy ID `YYYY.MM.DD.N+sha7`:
the UTC calendar date, N = the Nth deploy of that environment that day
(starting at 1), and the 7-character short SHA of the deployed commit. The
compute step derives N inside the deploy lane as one plus the count of that
environment's recorded deploy IDs created on that UTC date
(`scripts/deploy_id.py next`): only Deployment rows whose description is a
deploy ID count, since Actions auto-creates `environment:` rows (each deploy
job's own run included) that carry none. The lanes serialize deploys, so the
count cannot race. A malformed ID, or an ID naming another commit, refuses
before anything is rendered or snapshotted.

The staging deployment ID is stamped into the image as `BOT_DEPLOY_ID` next to the
revision and build ID, compiled into the binary, and reported on `/readyz` as `deploy_id`
(which the gates judge) and on the always-200 `/version` endpoint as
`{"deploy_id", "sha", "build_id"}` (which judges nothing, so deploy records
can read what serves without judging readiness). Ordinary production promotion
retains that original stamp: its newly computed production event ID appears in
the GitHub Deployment record, not in the unchanged binary. Live-SHA recovery
builds/stamps its own ID. The Worker proxies `/version` like `/readyz`.
A build from before stamping reports `unknown`;
rollback mode ignores the reported ID because the restored build carries its
original stamped ID. Deploy mode likewise judges the legacy
revision/build-ID shape for a guarded SHA whose Dockerfile predates the
`BOT_DEPLOY_ID` ARG, since that binary can never report a deploy ID.

After the gates judge, the record step stores the deploy as a GitHub
Deployment with `environment` set to `staging` or `production`, the deploy
ID as its description and `{"deploy_id"}` as its payload — never a tag or
Release — and posts its `success`/`failure` status with links to the run and
the Worker. A rollback mints a fresh event ID and names itself in the
payload, since its event ID differs from the restored build's stamped ID.
The record step runs after the auto-rollback, so a record failure fails the
run red with the service already restored: bookkeeping never gates
recovery. `scripts/ci/test_workflows.py` pins the compute/record step
shapes and their order against the stamp, gate and rollback steps.

## Production ownership takeover (P2–P4)

Implements P2–P4 of the approved design
([cutover-production-takeover-design.md](cutover-production-takeover-design.md)).
Staging keeps its own client and behavior; nothing here renames, repoints, or
relaxes the staging gate. This step never activates production: GO stays the
cutover lead's separate decision on the B4 execution card
([cutover-sequence.md](cutover-sequence.md) §§4–5).

**Requesting it.** A deploy ALWAYS takes over, manual or automatic: a
manual deploy can never leave production fenced (10-20 13:31Z: a manual
dispatch without `takeover: true` served 503 `ownership_fenced`/`not_owner`
for 23 minutes). `takeover` now defaults to `true`, and `takeover: false` no
longer skips a deploy's takeover (the guard prints a notice). The only way to
deploy without the takeover is the explicit B4 cutover hold,
`cutover_hold: true` (deploy mode only, default `false`): the guard records
`Takeover: SKIPPED by cutover_hold: the ownership fence STAYS HELD` in the
run summary, the job deploys and stops, fenced answers carry no build
fields, and the gate fails without being a rollback signal. In rollback mode
`takeover` still selects the post-restore ownership read and confirmation.

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
   the recorded `NEW_VERSION` with `running=false`. For about 330 s (34 reads,
   10 s apart) it waits out the version change: a `503 deployment_mismatch` or
   an answer still naming the previous version is read again, because the
   ownership Durable Object keeps running the previous code until
   Cloudflare's deferred update reaches it (up to about 300 s, observed on
   2026-10-10 after both a deploy and a secret change). A mismatch that outlasts the window, and every other refusal or a
   running container, is NO-GO and the run stops before any POST. The validated,
   supported-field response is tee'd to the run log as the P2 receipt; arbitrary
   remote fields are discarded (see the [control response contract](cutover.md#authenticated-control-contract)).
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
forwarded into the container. `PRODUCTION_WORKER_URL` (repository-level
non-secret variable — see item 2 above for why the guard needs it at
repository scope) must be `https://` and differ from `STAGING_WORKER_URL`; the
job and the client both refuse otherwise, and the client never follows
redirects.
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
- accepts a rollback with no reviewers only when its `rollback`, `image` and
  `sha` match a verified rollback snapshot (see **Roll back**); any other
  rollback target still needs a human reviewer;
- never applies to a live-SHA takeover redeploy (`allow_unstaged`): it waives
  the staging history the automated approval vouches through, so it still
  needs the human Environment reviewers and is refused until one is added;
- records `Approval: automated (PRODUCTION_AUTO_APPROVE)` (or, for a
  snapshot-backed rollback, `Approval: automated rollback`) in the run summary.

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

The cutover executor records each checkpoint with
`scripts/cutover_watch_checkpoint.py` — one checkpoint per call — and pastes
the emitted row onto the execution card at `T_0` (first `/readyz` 200 on the
production revision) through the watch deadline `T_0 + 48 h`:

```sh
python3 scripts/cutover_watch_checkpoint.py --checkpoint +15m \
    --expected-sha <40-hex> --expected-build-id <run-id>-<attempt> \
    --production-url https://<production-worker>/ \
    --format markdown
```

`--checkpoint` is one of the five labels `+15m`, `+1h`, `+6h`, `+24h`,
`+48h`, matching the five checkpoint rows below; `--expected-sha` is the
deployed commit from the watch header and `--expected-build-id` is that
deploy run's `<run id>-<attempt>` from the run summary. The script is
read-only: one GET to `/readyz`, no writes, migrates, or DB connections. It
emits GO only on a 200 with every component ready and an exact
revision/build-ID match; anything short of a full match is EXTEND, never
GO, and a ROLLBACK decision stays human. Checkpoints follow
[cutover.md](cutover.md) §48-hour watch. Poll read-only on a short cadence
(suggested 60 s); record findings, not every healthy poll.

`--format json` emits the same checkpoint as a one-line machine receipt
instead of the table row, with stable `reason` ids and a `next_action`:

```json
{"checkpoint": "+15m", "verdict": "GO", "reason": "ready_match", "reason_detail": "/readyz 200: all 4 components ready, revision matches the SHA, build 700-1", "origin": "https://two-bot-next-production.<sub>.workers.dev", "expected": {"build_revision": "<40-hex>", "build_id": "<run>-<attempt>"}, "observed": {"build_revision": "<40-hex>", "build_id": "<run>-<attempt>"}, "next_action": "paste the GO row onto the execution card and continue the watch"}
```

### Watch header (record once at T_0)

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
  `automod_executor_failed`, `raid_executor_failed`,
  `gateway_runtime_failed`, `gateway_task_panicked`,
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
| REST 5xx and action latency | Single-attempt calls keep their 5 s body-read deadline; 429 `retry-after + 250 ms` honored, 5xx backoff 500…8000 ms across the bounded retry budget; moderation uses one timed attempt; 5xx at most 10% of REST requests between alert-rule samples (minimum 10 requests) | Any uncertain send without a recorded disposition, a route whose `job_consecutive_failures` reaches 3, or the `rest_5xx_rate` alert firing on the deployed revision: freeze that writer and reconcile; roll back if the failing path shipped in this revision | [rest-guard.md](rest-guard.md); [pacing-backoff-acceptance.md](pacing-backoff-acceptance.md); [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md) Alert: REST 5xx |
| Bot token | Zero 401s on bot-authenticated endpoints | First latched `token_invalid`: stop retries, freeze writers; no rollback until provisioning is corrected through the governed path | [rest-guard.md](rest-guard.md) |
| Unban queue | Sweep every 30 s (`UNBAN_SWEEP_INTERVAL_SECONDS`), at most 25 jobs claimed per sweep; no due unban left pending across sweeps without a named disposition | Any overdue sanction without a named disposition is a finding; zero unexplained overdue is required for GO at each checkpoint | [member-moderation.md](member-moderation.md); [cutover.md](cutover.md) §§T-minus, 48-hour watch |
| Scheduled jobs | Scheduled jobs: last success within twice the cadence; fewer than 3 consecutive failures. Unban sweep, automod expiry and event-driven session checkpoint: manual checks below | Installed `job_stale` covers only registered cadences, `ticker_stale` covers the two 15 s tickers, `job_consecutive_failures` covers labelled failed completions per job (including `member_unban_sweep` and `automod_expiry` on their own labels), and the coarse `job_unknown_stale` ticket covers a stale nonzero success on any label without a cadence entry. `session_checkpoint` is stale-exempt and records successes only in these job series. A manual failure/stall finding has the same response: freeze the consumer, fix the dependency; roll back if the regression shipped in this revision | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [manual checks](#scheduled-job-manual-checkpoints) |
| DB pool | Idle connections above zero, below max | Pool at max with zero idle for 3 consecutive keepalive samples: do not restart to free it; freeze writers, fix the holder; roll back if a new query path holds checkouts | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md) Alert: DB pool |
| `db_errors` | Fewer than 3 storage failures between keepalive samples; a counter reset (process restart) skips the window, not proof of health; sustained low-rate failures surface through `job_consecutive_failures` | 3 or more storage failures between samples: correlate the `op` label and recent deploys; do not run SQL probes or restart to clear errors; escalate repeated bursts per the runbook, evaluate rollback if this revision introduced the failing writes | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md#alert-db-errors) |
| `send_admission_blocked` | Fewer than 3 consecutive keepalive samples with new admission refusals; a sample with no new refusals breaks the streak; admission SQL failures count in `db_errors`, not refusals | New admission refusals in 3 consecutive samples: investigate cooldowns and held lanes; do not replay uncertain sends or restart to free the lane; escalate persistent refusals per the runbook, evaluate rollback if this revision introduced the regression | [metrics.md](metrics.md#off-container-scrape-and-alert-rules); [runbook.md](runbook.md#alert-send-admission-blocked) |
| RSS and placement | RSS near the B1 soak-measured floor (~140 MiB, under the ~200 MiB `lite` gate signal) on the shipped `basic` placement; image/binary sizes inside the B1 ceilings (25% image and 40% binary headroom policy) | Sustained RSS growth versus the B1 floor with no attribution, sustained use pressing the placement cap, or any OOM-kill: freeze writers, investigate or roll back | [b1-baseline.md](b1-baseline.md) (`basic` verdict, ceilings); [cutover.md](cutover.md) §48-hour watch; this is a separate production-watch signal, not B2's numeric RSS acceptance |
| Event continuity | Zero unexplained gaps or duplicated effects versus independent moderator observations | Any unexplained gap or duplicated execution is a stop condition: freeze writers, evaluate rollback | [cutover.md](cutover.md) §48-hour watch; [staging-soak.md](staging-soak.md) acceptance |
| Shutdown drain | SIGTERM drain completes inside 35 s (`SHUTDOWN_TIMEOUT_SECONDS` default) | `shutdown_deadline_exceeded` (exit 1): the restart reads the last committed checkpoint; repeated misses block GO until investigated | [configuration.md](configuration.md) |

### Scheduled-job manual checkpoints

At +15 min, +1 h, +6 h, +24 h and +48 h, inspect the current checkpoint's
container logs with these exact JSON-field filters (not the `msg` envelope):

```text
message = "periodic job failed" AND job = "member_unban_sweep"
message = "member unban sweep completed jobs"
```

Keep each failure's timestamp and bounded `error_class`; the completion line carries
`completed` only when work was actually done. Do not copy guild/member IDs into
the watch log. Zero-work successes emit no completion line, and a stalled
attempt emits no failure until it finishes or times out, so **no matching log
line is not success**. Inspect the saved `/readyz` response too:

```sh
jq '.jobs.member_unban_sweep | {running, parked, last_success, consecutive_failures}' readyz.json
```

`last_success` is Unix **milliseconds** (unlike metric timestamps). Compare it
with the read timestamp and prior checkpoint: a parked/missing job is not an
accepted sweep; three failures or no recent success needs a disposition. The
30 s cadence is not an attempt deadline: a sweep may run for up to 180 s, so
investigate a >60 s age with `running`/logs before declaring it stalled.
`job="member_unban_sweep"` is valid per-sweep evidence: `job_consecutive_failures`
fires at 3 on its own label and a stale nonzero success tickets via the coarse
`job_unknown_stale` fallback. `job="other"` remains the shared unknown-name
catch-all, never per-sweep evidence.

For automod expiry, inspect the saved `/readyz` response (there is no
completion log line: the 60 s tick is a no-op success until the gateway builds
the activation, and it only does real expiry work when `TWO_AUTOMOD=1`):

```sh
jq '.jobs.automod_expiry | {running, parked, last_success, consecutive_failures}' readyz.json
```

When `TWO_AUTOMOD` is not `1` the job is not registered and the entry is
absent: record the checkpoint as not-applicable, never green. Otherwise compare
`last_success` (Unix **milliseconds**) with the read timestamp and prior
checkpoint like the unban sweep: three failures or no recent success needs a
disposition. `job_consecutive_failures` fires at 3 on the `automod_expiry`
label and a stale nonzero success tickets via the coarse `job_unknown_stale`
fallback.

For the event-driven session checkpoint, use the
[manual checkpoint queries](voice-cutover-rollback-triggers.md#manual-checkpoint-queries)
for success age, commit traffic and commit failures; there is no periodic
"two cadences" stale rule. For uninstalled multi-window burn alerts, evaluate
[the manual burn windows](error-budget-alerts.md#manual-checkpoint-evaluation)
at each checkpoint. Record missing history or signals as unknown, never green
because an alert stayed silent.

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
