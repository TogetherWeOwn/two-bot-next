# Staging signed attendance acceptance

After Web Ember M0a and the Bot production prerequisites, use the candidate
serving the signed verb in a protected staging window. Keep the same compiled
revision, build ID, deploy ID and tenant. For each Worker version, use its
verified [staging rollout receipt](staging-rollout-gate.md).

Use an isolated synthetic store. Give `attendance-acceptance:<build_id>` to
a pending event in TWO Staging. Use a recent UTC whole-minute start, end 30
minutes later, and seed one closed 12-minute session starting one minute
after the start. Use fabricated member `300000000000000001` and room
`400000000000000001` with observed name `Synthetic attendance room`. Finalize
the same event and wait for grace to elapse. Seed the equivalent finalized
record at the `:foreign` suffix key only in a different synthetic tenant.
Use the same finalized primary event for OFF. The wire request has no guild
selector, so the foreign-only key tests the configured reader's tenant scope.

Put trusted configuration and fixture readback from that window into `observed`
JSON. The harness probes `/readyz` around each signed read.

| Fields | Expected observation |
| --- | --- |
| `environment`, `origin`, `guild_id` | `staging`, the canonical staging origin, configured TWO Staging guild ID as a string |
| `worker_version`, `build_revision`, `build_id`, `deploy_id` | Verified rollout identity |
| `attendance_read` | Effective `TWO_INTERNAL_ALLOW_ATTENDANCE_READ`. JSON `true` for pending and finalized, `false` for OFF and cleanup |
| `attendance_capture`, `signed_ingress` | Capture `false` throughout. Signed ingress `true` during the three wire phases |
| `primary_fixture`, `foreign_fixture` | Primary `pending` or `finalized` per phase. For finalized, `foreign-only` after checking the key exists only in the foreign tenant |
| `lifecycle`, `fixture_rows` | Successful `staging-use.mjs` JSONL `ready` record for wire phases. Stopped `release` or `stop` record and `0` fixture rows across both tenants for cleanup |

Use the same fixture start (`YYYY-MM-DDTHH:MM:00Z`) and each phase's readback.
Inject `STAGING_ATTENDANCE_KEY_ID` and `STAGING_ATTENDANCE_SIGNING_SECRET` for
the dedicated staging caller. Pass the 64-character lowercase hex secret as ASCII.

```sh
python3 scripts/staging_attendance_acceptance.py "$phase" \
  --staging-url "$STAGING_WORKER_URL" --rollout rollout.json \
  --observed observed.json --fixture-start "$fixture_start" --execute
```

- `pending` requires signed `404 not_found` without a result.
- `finalized` requires the exact event-bound v1 `final: true` result and a signed `404 not_found` for the foreign key.
- `off` requires signed `403 action_not_allowed` while the fixture stays finalized.
- `cleanup` checks OFF, stopped and fixture removal offline. Omit `--execute` and `--fixture-start`.

Exit `0` means pass, `1` means fail, and `2` includes preflight without
`--execute`. Run cleanup even after failure. Record rollout and phase verdicts
without credentials, signing headers or raw responses. CI uses synthetic envelopes.
