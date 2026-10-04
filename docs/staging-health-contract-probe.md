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

## Migration-level assertion

The probe reads migration drift through the contract's own vocabulary, not
through SQL: `gateway_failure durable_gateway:checkpoint_load_failed` is the
DB-behind-binary (staging rollout-timeout) signature and fails with the
`db-behind-binary` reason (`docs/voice-cutover-rollback-triggers.md` T1).
The response is migrate-before-redeploy, never a blind re-run.

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
the db-behind-binary signature; a 200 that contradicts its breakdown;
ownership refusals, short breakdowns and non-JSON bodies as not-the-bot;
health 503 as not-liveness; transport failures by class; redirects observed,
never followed.

## Not covered here

- Gateway reconnect/resume behavior (separate leaf, done).
- The redirect, automation-read and rollout surfaces (their own smokes).
- The full staging suite (parent suite card).
- Migration apply or any database write (`staging-migrate` owns the governed path).
- The run-record template itself (sibling leaf).
