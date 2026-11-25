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

## Enablement and rollback evidence

Keep attendance enablement on hold until Web Ember M0a and the qualified
Bot release are in place and the applicable
[protected release checks](../CONTRIBUTING.md#production-release-gate) pass.
Tie each verdict to the protected run, the verified serving revision, build
and deploy IDs, and the tenant. Read back the effective controls on that
runtime and keep only sanitized outcomes in the evidence.

- **Bot capture.** Read back capture enabled. Verify that the runtime processes
  an eligible fresh voice observation in a mapped synthetic event window and
  writes a new attendance span linked to that window in the isolated store.
  After rollback, read back capture OFF. Verify that the runtime processes a
  fresh, otherwise eligible mapped observation and that storage readback shows
  no new attendance window or span for it. Confirm a previously finalized
  fixture remains stored before cleanup.
- **Bot signed reader.** Read back active private signed ingress for the
  tenant and effective `TWO_INTERNAL_ALLOW_ATTENDANCE_READ`. Use the staging
  recipe above with capture OFF to prove the serving receiver dispatches
  `event.attendance.read`. Pending and foreign keys return `404 not_found`,
  and the finalized key returns the exact event-bound v1 `final: true` result.
  After rollback, read back the reader OFF while signed ingress remains active
  for the check. A valid signed read of the same retained finalized key must
  return `403 action_not_allowed` without a result. Then verify the separate
  stopped, released and fixture-removal cleanup receipt.

The Web caller has its own pull control. When rolling it back, verify its
effective OFF state separately and confirm that it stops issuing new pulls.
Keep the already-frozen Echo. If either Bot OFF check lacks its positive
control (a processed eligible observation or the previously successful
signed read of the retained fixture), leave that rollback verdict unverified.

Apply the [no-backfill contract](bot-site-contract.md#9-echoes--attendance-record-v1-4-carried-two-additions).
A completed pre-enable window stays a Web sign-up record. A delayed
observation after grace leaves its signed key at `not_found`, even after
capture is enabled. Use a later eligible window to prove new capture.

As source evidence, [#2522](https://github.com/TogetherWeOwn/two-bot-next/pull/2522)
covers mapped capture dormancy, public listener `404`, unsafe startup bind
and guild refusals, and ordinary signed receiver refusal even with the read
flag set. The flag alone leaves the documented
[receiver route unmounted](internal-actions-receiver.md#container-wiring).
[#2417](https://github.com/TogetherWeOwn/two-bot-next/pull/2417) exercises both
enablement and rollback orders with a synthetic Web peer and verifies that a
fresh signed read after re-enablement resolves the same finalized record.
[#2491](https://github.com/TogetherWeOwn/two-bot-next/pull/2491) covers the
pre-enable window. Use protected runtime evidence for deployed readiness.
