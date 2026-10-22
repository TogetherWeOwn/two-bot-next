# Bounded subprocess checks

One local policy bounds every Git and cargo-metadata child wait in CI
selectors, gates and the retention audit, so a stalled child fails closed
here instead of hanging until the external CI ceiling.

## Policy

- CI tools (`check-docker-manifests.py`, `check-migrations.py`,
  `check_parity_baseline.py`, `ci/check-floor-guard.py`,
  `ci/check-squawk-migrations.py`, `container-inputs.py`, `job-inputs.py`):
  each `subprocess.run` carries `timeout=30` (one value per tool).
- Retention audit (`cargo_cache.py` `retention_audit`): each of the three
  provenance Git waits (`rev-parse`, `ls-files`, `check-ignore`) carries
  `timeout=min(30, live remaining receipt freshness)`, recomputed before
  each call as static age minus monotonic elapsed since audit entry; under
  one second of live freshness the audit refuses instead of probing,
  each candidate's first call rechecks freshness before probing, and
  freshness is rechecked after the waits.
- Every timeout, spawn/OS failure and nonzero child result maps into the
  tool's existing fail-closed vocabulary with the exit status and a `next:`
  action. No traceback, no echoed child stderr. Selector failures exit 2
  with no selection line (full build / full jobs); the supervised Cargo
  `Popen` is intentionally excluded.

## Checks (offline, no Cargo pool, no CI dispatch)

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_bounded_subprocess_ci_acceptance.py'
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_bounded_subprocess_cache_acceptance.py'
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache*.py'
git diff --check
```

Neighboring suites (must stay green): `test_check_floor_guard`,
`test_check_squawk_migrations`, `test_container_inputs`, `test_job_inputs`,
`test_check_docker_manifests`, `test_parity_baseline`, `test-check-migrations`.
