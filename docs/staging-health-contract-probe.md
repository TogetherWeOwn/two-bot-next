# Staging health-contract probe (read-only E2E)

`scripts/staging_health_contract_probe.py` asserts the bot container's
health contract against the staging Worker origin through the public HTTP
surface only (`crates/bot/src/server.rs`, `crates/core/src/health.rs`). It
is stdlib-only, sends no credentials, follows no redirects, performs no
writes, and refuses any non-staging origin before a request is sent. The
offline fixtures (`scripts/test_staging_health_contract_probe.py`) run in
CI; the live run is manual-dispatch only.

## Contract under test

| Check | Probe | Pass shape |
| --- | --- | --- |
| `health` | `GET /health` | Exact 200 with `{"status":"ok"}` and no redirect target. Process liveness only: a 200 never claims readiness. |
| `readyz` | `GET /readyz` | 200 or 503 with the bot's component breakdown (`process`, `gateway`, `database`, `token_invalid`, each `ready`/`starting`/`down`), the informational `jobs` object, an optional `gateway_failure` in the fixed `durable_gateway` + 12-class vocabulary, and non-empty compiled `build_revision`/`build_id`. The code must equal 200 iff every component is ready. A shape-correct 503 is still a probe FAIL: a truthful parked process is never E2E approval. |
| `build` | readyz body | `build_revision`/`build_id` present; when `--expected-sha` (or `$EXPECTED_SHA`) is given, the revision must equal the deployed commit SHA from the `deploy-staging` receipt. |

`/healthz` at the staging origin is the Worker's redirect probe (`ok`),
covered by the redirect smoke — not this probe.

## Checkpoint-read assertion (not a root-cause diagnosis)

`gateway_failure durable_gateway:checkpoint_load_failed` identifies only
**the failed checkpoint read step** (`crates/bot/src/gateway_failure.rs`,
`FailureClass::CheckpointLoadFailed`). It does not distinguish schema lag,
ACL denial, or connectivity failure. Readiness stays FAIL; neither this
class nor a rollout timeout proves the database is behind the binary.

A root-cause claim requires an **independent, reviewed receipt** with
schema/ledger, checkpoint-reader ACL, or connectivity evidence tied to the
affected staging target and deployed build. Review those separate evidence
sources before choosing remediation; do not recommend applying migrations
solely from this failure class. See
[`voice-cutover-rollback-triggers.md` T1](voice-cutover-rollback-triggers.md#t1-checkpoint-read-failure-staging-rollout-timeout-lesson).

The allowlisted JSON evidence retains `gateway_failure_class` and the
failing `readyz` check's observed-step reason. Its additive
`gateway_failure_root_cause` field is `"unverified"` for this class and
`null` otherwise; it is never copied from remote cause/error details.
The probe cannot create the independent root-cause receipt.

**No probe is allowed to apply a migration or test production.** Migration
execution remains a separate reviewed, governed staging operation, not
an action or recommendation inferred by this health probe.

## Live run (manual, staging only)

```sh
STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \
EXPECTED_SHA=<deployed commit SHA from the deploy-staging receipt> \
  python3 scripts/staging_health_contract_probe.py [--evidence FILE]
```

Origin fence: anything but an
`https://two-bot-next-staging.<sub>.workers.dev` origin — including the
production Worker, plain HTTP, credentialed URLs, or any path/query —
refuses with exit 2 and sends no request. Record the run with the
run-record template (sibling leaf owns the template; this probe consumes
it, referencing it without blocking on it).

## Verification (offline, runs in CI)

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_staging_health_contract_probe.py -v
```

Fixtures cover: production/HTTP/credentialed/missing origins refusing
before any request; the all-ready pass with build identity and evidence;
`--expected-sha` match and mismatch; parked gateway as truthful-not-approval;
checkpoint-read failure without a root-cause assertion or migration advice;
unreviewed remote causes/details dropped from output and evidence; unknown
failure classes refused without echoing details; a 200 that contradicts its
breakdown; ownership refusals, short breakdowns and non-JSON bodies as
not-the-bot; health 503 as not-liveness; transport failures by class;
redirects observed, never followed.

## Not covered here

- Gateway reconnect/resume behavior (separate leaf, done).
- The redirect, automation-read and rollout surfaces (their own smokes).
- The full staging suite (parent suite card).
- Migration apply or any database write (`staging-migrate` owns the governed path).
- The run-record template itself (sibling leaf).
