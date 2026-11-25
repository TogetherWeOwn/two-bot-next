# Staging signed attendance acceptance

After Web Ember M0a and the Bot production prerequisites, use the candidate
serving the signed verb in a protected staging window. Keep the same compiled
revision, build ID, deploy ID and tenant. For each Worker version, use its
verified [staging rollout receipt](staging-rollout-gate.md).

In an isolated synthetic store, map `attendance-acceptance:<build_id>` to a
pending TWO Staging event with a recent UTC whole-minute start and a 30-minute
window. Seed a closed 12-minute session starting one minute in for fabricated
member `300000000000000001` and room `400000000000000001` named `Synthetic attendance room`.
Finalize after completion and 15 minutes of grace (start + 45 minutes). Use that
event for OFF. Seed an equivalent finalized record at `:foreign` only in a
different synthetic tenant to test the reader's tenant scope.

Put trusted configuration and fixture readback from that window into `observed` JSON.

| Fields | Expected observation |
| --- | --- |
| `environment`, `origin`, `guild_id` | `staging`, the canonical staging origin, configured TWO Staging guild ID as a string |
| `worker_version`, `build_revision`, `build_id`, `deploy_id` | Verified rollout identity |
| `attendance_read` | Effective `TWO_INTERNAL_ALLOW_ATTENDANCE_READ`. JSON `true` for pending and finalized, `false` for OFF and cleanup |
| `attendance_capture`, `signed_ingress` | Capture `false` throughout. Signed ingress `true` during the three wire phases |
| `primary_fixture`, `foreign_fixture` | Primary `pending` or `finalized` per phase. For finalized, `foreign-only` after checking the key exists only in the foreign tenant |
| `lifecycle`, `fixture_rows` | Successful `staging-use.mjs` JSONL `ready` record for wire phases. Cleanup requires a stopped `release` record with lease `released` in the matching owner epoch and `0` fixture rows across both tenants |

Use Python 3.11+, the same fixture start (`YYYY-MM-DDTHH:MM:00Z`) and each phase's readback.
Inject `STAGING_ATTENDANCE_KEY_ID` and a dedicated `STAGING_ATTENDANCE_SIGNING_SECRET` of 64 lowercase hex characters as ASCII.

```sh
python3 scripts/staging_attendance_acceptance.py "$phase" --rollout rollout.json \
  --observed observed.json --fixture-start "$fixture_start" --execute
```

- `pending` requires `404 not_found` without a result.
- `finalized` requires the exact event-bound v1 `final: true` result and `404 not_found` for the foreign key.
- `off` requires `403 action_not_allowed` while the fixture stays finalized.
- `cleanup` checks OFF, stopped and fixture removal offline. Omit `--execute` and `--fixture-start`.

The v2 fixture lists the `not_found` outcome. The [Web reader](https://github.com/TogetherWeOwn/two-web-next/blob/cba63716d12421bcf054240cbfe8b036861bce62/src/bot/attendance-read.ts#L17-L109) consumes it as a `404` error and allows 2.5 seconds per signed read.

Exit `0` means pass, `1` means fail, and `2` includes preflight without `--execute`.
Run cleanup even after failure. Record verdicts without credentials, signing headers or raw responses.
