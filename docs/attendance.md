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

## Compare one rehearsal offline

Keep the full sanitized `lifecycle` record in each phase's observation. Save
the four observations in `rehearsal.json` as one JSON object keyed by `pending`,
`finalized`, `off` and `cleanup`. Each value is the observed JSON object for
that phase. Use the rollout receipt from the original pending phase.

```sh
python3 scripts/staging_attendance_acceptance.py rehearsal --rollout rollout.json \
  --observed rehearsal.json
```

The comparison requires strictly increasing `observedAt` timestamps from the
trusted staging client in pending, finalized, OFF and cleanup order. The client
stamps each successful observation in UTC at millisecond precision. Tied
timestamps leave the order unverified. Each `ready` record must leave enough
time in the active lease for the probe budget. A stopped `release` may be
observed after expiry as recovery for that same lease.

All four phases must match the pending lease's owner epoch, `runId`,
`runAttempt`, `acquiredAt` and `expiresAt`. A later lease on the same rollout
may retain the same owner epoch. Its stopped release and zero fixture rows
belong to that later rehearsal. If the original lease has no matching cleanup
receipt, leave its cleanup verdict unverified.
Record `PASS: attendance rehearsal` alongside the separately recorded signed
probe verdicts from the protected run.

## Enablement and rollback evidence

Keep attendance enablement on hold until Web Ember M0a and the qualified
Bot release are in place and the applicable
[protected release checks](../CONTRIBUTING.md#production-release-gate) pass.
Tie each verdict to the protected run, the verified serving revision, build
and deploy IDs, and the tenant. Read back the effective controls on that
runtime and keep only sanitized outcomes in the evidence.

- **Bot capture.** The signed-reader recipe keeps capture OFF. Keep capture
  readiness unverified until a separate protected check can use an exclusive
  staging window, synthetic identities and an isolated store. Read back the
  qualified Bot release's effective capture control as enabled. Verify that
  the runtime processes an eligible fresh voice observation in a mapped
  synthetic event window and writes a new attendance span linked to it.
  After rollback, read back capture OFF. Verify that the runtime processes a
  fresh, otherwise eligible mapped observation and that storage readback shows
  no new attendance window or span for it. Confirm a previously finalized
  fixture remains stored before cleanup. Verify removal of the synthetic
  capture fixtures at cleanup.
- **Bot signed reader.** Read back active private signed ingress for the
  tenant and effective `TWO_INTERNAL_ALLOW_ATTENDANCE_READ`. Use the staging
  recipe above with capture OFF to prove the serving receiver dispatches
  `event.attendance.read`. Pending and foreign keys return `404 not_found`,
  and the finalized key returns the exact event-bound v1 `final: true` result.
  After rollback, read back the reader OFF on the same serving receiver build
  and tenant that passed the finalized read, with signed ingress still active.
  A valid signed read of the same retained finalized key must return
  `403 action_not_allowed` without a result. Then verify the stopped, released
  and fixture-removal cleanup receipt for the originating lease with the offline
  comparison above.

The Web caller has its own pull control. When rolling it back, verify its
effective OFF state separately. Confirm it issues no new pulls during an
eligible synthetic polling opportunity while Bot can still serve the finalized
result. Preserve any already-frozen Echo. If a rollback check lacks its
behavioral evidence, leave that control's OFF verdict unverified.

Apply the [no-backfill contract](bot-site-contract.md#9-echoes--attendance-record-v1-4-carried-two-additions).
A completed pre-enable window stays a Web sign-up record. A delayed
observation after grace leaves its signed key at `not_found`, even after
capture is enabled. Use a later eligible window to prove new capture.

As source evidence, [#2522](https://github.com/TogetherWeOwn/two-bot-next/pull/2522)
covers mapped capture dormancy, public listener `404`, unsafe startup bind
and guild refusals, and the ordinary signed receiver's refusal of the verb
even with the read flag set. See the documented
[receiver wiring](internal-actions-receiver.md#container-wiring).
[#2417](https://github.com/TogetherWeOwn/two-bot-next/pull/2417) exercises both
enablement and rollback orders with a synthetic Web peer and verifies that a
fresh signed read after re-enablement resolves the same finalized record.
[#2491](https://github.com/TogetherWeOwn/two-bot-next/pull/2491) covers the
pre-enable window. Use protected runtime evidence for deployed readiness.

## Recover the capture-only handoff

Use this evidence handoff when a separately qualified capture-only state
precedes the protected signed-reader rehearsal. The enablement hold above
applies. Use the existing protected release and staging recovery paths for
runtime action. [Staging recovery](staging-rollout-gate.md#full-window-workflow-and-recovery-semantics)

The merged source trail covers the retained `0475` recovery fence, the
conservative observation floor and the dormant mapped writer.
[#2158](https://github.com/TogetherWeOwn/two-bot-next/commit/28990125adbc3a636c6a70e9a99d8dbd9f77fbcd)
· [#2306](https://github.com/TogetherWeOwn/two-bot-next/commit/8374f89465bb27e9caee57577b2e966e709a1427)
· [#2338](https://github.com/TogetherWeOwn/two-bot-next/commit/6dad6cddabca0ce882dee5863789f69d813882ad).

1. Before the reader window, pin the prior capture-only rollout and its
   effective capture ON, reader OFF and ingress state. Attach its qualified
   artifact, runtime and tenant identifiers and the synthetic capture evidence
   described above. Record the reader candidate's verified rollout and link its
   protected run, original lease acquisition record, acceptance tool source
   SHA and synthetic fixture scope to that baseline. Keep prior capture
   evidence outside the reader fixture cleanup scope. Link the capture OFF
   readback and behavioral evidence described above to the handoff before
   the first signed probe. Use the reader acceptance and offline comparison
   above for the capture-OFF window.
2. If a reader probe fails or cleanup is invalid, keep the transition
   unqualified. Preserve the failed or missing verdict in the protected run
   record and use the existing recovery path. When the original reader window
   still permits the OFF check, keep its finalized fixture through the fresh
   signed refusal check. Leave that verdict unverified if the check or its
   original fixture is unavailable.
3. Retain two affirmative receipts in the protected run evidence. Bind the
   reader cleanup to the original lease and reader fixture scope using the
   stopped release and zero-row readback above. If a probe failed before all
   phase evidence was recorded, check the standalone cleanup result against
   the saved rollout and original lease acquisition record, comparing every
   lease field listed above. Verify that the zero-row readback covers the
   pinned reader fixture scope in both tenants. Leave cleanup unverified if
   any part of that match is missing.
4. Confirm the original reader window stopped before opening a restoration
   window. Use the staging lifecycle's recovery if the stop is unconfirmed.
   After the recovery action takes effect, verify the serving rollout and
   read back effective capture ON, reader OFF and the prior ingress state
   on the pinned prior artifact and tenant. Record these fresh observations
   with their UTC times and the protected restoration run and attempt. Use the
   capture ON behavior above to confirm a fresh eligible synthetic observation
   in that window. Its fixtures follow that window's cleanup. Leave restoration
   unverified if the rollout or control readback cannot be attributed to the
   post-recovery window.

The protected run record carries the sanitized baseline, phase evidence,
failure and receipts through closeout. If recovery continues in another
protected run, cross-link both records and retain the original lease and
fixture scope on its cleanup evidence. A later run's own release or zero-row
readback cannot close the original window. Record restoration and cleanup
verdicts independently. A missing receipt leaves its verdict unverified.
