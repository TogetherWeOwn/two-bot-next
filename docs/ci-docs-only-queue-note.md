# Docs-only PR check-queue wait: measurement note

Follow-up read-only slice to the path-scoped fast-pass (#434).
No workflow changed in this note.

Status: the SBOM gate this note first pointed at shipped as #522 while
the note was in review. Sections marked "pre-#522" describe the sampled
runs; "post-#522" is one later observation. The recommended slice below
shipped with the `check` lane split; see "Update: lane split" at the end.

## Method

- Sample: every merged PR numbered #422–#462 that merged on 2026-10-03
  (UTC) and has a `check` run on its merged head SHA: 27 PRs. All of
  them ran before #522 (merged 2026-10-04 06:48Z). The window straddles
  the fast-pass merge (#434, 21:23Z).
- Classification: each PR's file list run through the `job-inputs.py`
  and `container-inputs.py` selectors as they stood at the PR head.
  - docs-only (12): no `rust`, no `parity`, no image build. #429, #433,
    #440, #441, #444, #446, #447, #449, #451, #453, #458, #461.
    Eleven touch only `docs/`; #444 also touches `wrangler/test/`.
  - code (13): selector picks `rust`, `parity` or an image build.
    #422, #425, #426, #428, #430, #431, #432, #434, #435, #437, #448,
    #450, #455. (#455 edits `docs/parity.md`, which Rust tests read.)
  - worker-config (1): #462 changes only `wrangler/wrangler.toml`, an
    exact image input. Shown separately, not in either aggregate.
  - excluded (1): #423 is a docs edit whose run is a re-run record
    (attempt 2, negative timestamps).
- Queue wait: per job, `started_at - created_at` from the Jobs API
  (`filter=latest`) on the merged head's `check` run. Jobs that ran
  (success or failure) only; skipped jobs have no queue. A job gated by
  `needs` is created when it becomes runnable, so dependency waits are
  not counted as queue.
- Wall clock: earliest job `created_at` to latest job `completed_at`.
- Percentiles use linear interpolation.

## Results (pre-#522)

Queue wait is negligible for both groups on hosted runners, so the
fast-pass saves wall clock, not queue position.

| Group | Jobs timed | Median job queue | p90 job queue | Median per-PR max queue | p90 per-PR max queue |
|---|---|---|---|---|---|
| docs-only (12 PRs) | 84 | 2.0 s | 3.0 s | 3.0 s | 9.4 s |
| code (13 PRs) | 160 | 2.0 s | 3.0 s | 4.0 s | 12.2 s |

| Group | Median wall clock | p90 wall clock |
|---|---|---|
| docs-only (12 PRs) | 8.1 min | 8.4 min |
| code (13 PRs) | 26.6 min | 27.8 min |

Heavy-job skips: all 12 docs-only runs skip exactly the same 6 jobs
(the five Rust DB jobs plus `parity-docs`); code runs skip 0 or 1.

Where a docs-only run spent its time (median job duration, 12 runs):

| Job | Duration |
|---|---|
| `PR SBOM dry-run / image scan and SBOM` | 386 s (max 412 s) |
| `check` | 78 s |
| `worker check` | 50 s |
| `PR SBOM dry-run / verify SBOM inventories and evidence` | 11 s |
| `container smoke` | 9 s |
| `job inputs changed`, `container inputs changed` | 6 s each |

- The SBOM image scan, not container smoke, dominated docs-only wall
  clock. It ran on all 12 runs because `supply-chain` had no gate.
- Container smoke was already cheap on docs-only PRs: `docs/` is in
  `SAFE_SKIP_PREFIXES` (`container-inputs.py`), so the build steps skip
  and the job takes about 9 s.
- `worker check` still runs on docs-only PRs by design: `docs/` changes
  select `worker` (`runbook.test.ts` asserts on the docs listing).

Worked examples:

- #461 (pure docs): container smoke 9 s; image scan and SBOM about
  6.5 min; this is the pre-#522 cost.
- #462 (`wrangler/wrangler.toml` only): not docs-only. The file pins
  the deployed image reference, so container smoke built the image
  (388 s) and the SBOM scan ran 364 s. This is the worker-config case.

## Post-#522 observation

#522 gated `supply-chain` on a new `supply` selector; `docs/` paths
select `supply=false` (`job-inputs.py`). One docs-only PR ran on the new
workflow: #129 (head `4f264ecc`, run 37186025663). `PR SBOM dry-run`
skipped and the whole run took 2.1 min (07:31:06Z to 07:33:15Z).

Its critical path is the `check` job at 106 s. Of that, 41 s is
"Initialize containers" (the `rust:bookworm` job container and the
Postgres service) and 22 s is the docker action image build in job
set-up; the offline guard steps take about 40 s. A docs-only PR does
not need the container or the database for those guards.

One sample is an observation, not a median. Re-measure once about ten
post-#522 docs-only PRs have merged.

## Where code-PR time goes (the larger lever)

Median job durations on the 13 code PRs: `check` 19.1 min, `container
smoke` 7.5 min, `self-role store leases` 7.1 min, SBOM image scan
5.9 min, the other DB jobs 2.6–3.1 min each.

Median code wall clock is 26.6 min, which is about 7.8 min plus
19.1 min. The `check` job starts a median 7.8 min after the run
starts and a median 3 s after the last of its `needs` finishes, so the
cargo suite is serialized behind the self-role and SBOM jobs.
`check` still declares
`needs: [self-role-store, supply-chain, parity-docs, job-inputs]`
(`check.yml` on main at `8f119e1c`, line 61).

## Recommended next slice (≤4h)

Run `check` in parallel with the DB and SBOM jobs.

- The `protect-main` ruleset now requires `pr-lint`, `gitleaks` and
  `ci-ok` (read from the branch rules API on 2026-10-04); `check` is
  no longer a required context. `ci-ok` already needs `check`,
  `worker`, `supply-chain`, `parity-docs`, `self-role-store` and the
  DB jobs, so every gate still blocks the merge.
- Change `check` to `needs: [job-inputs]` and drop its three "require
  ... to pass" steps for `parity-docs`, `self-role-store` and
  `supply-chain` (`check.yml` lines 166–176 on `8f119e1c`); `ci-ok`
  carries them. `scripts/ci/test_required_checks.py` pins the
  `required-checks` aggregator's `needs` (lines 64–70) and `check`'s
  `job-inputs` need (line 57), and nothing asserts that `check` needs
  the other three. Add the same `needs` pin for `ci-ok` so it cannot
  lose a dependency. The test's `REQUIRED` set (line 17) still lists
  `check` and `worker check`, which no longer matches the ruleset:
  reconcile it in the same slice.
- Expected effect: code-PR wall clock falls from about 26.6 min to
  about 19–20 min (bounded by `check` itself), roughly 7 min saved per
  code PR. Docs-only PRs are unchanged at about 2 min.
- Verify with a probe PR: `check` starts within about 30 s of the run
  start, and `ci-ok` stays red when a DB job is forced to fail.
- Risk: the DB jobs now run concurrently with `check`, which raises
  peak concurrent runner use per PR. Hosted runners for this public
  repo have no shared-host contention, but confirm the concurrency
  group still cancels superseded runs.

Not recommended as its own slice: the remaining docs-only `check`
set-up (about 1 min of container and service start). It is real but
small next to the code-PR serialization above.

## Update: lane split

The slice above shipped together with a split of the `check` job. `check`
is now the lint lane (offline guards, cargo-deny, fmt, both clippy runs)
and three more jobs run beside it: `rust-tests` (unit, binary,
integration, property and doc tests), `ignored-db-stores` and
`ignored-db-runtime` (the `--ignored` database suites, and the two backup
CLI regressions in the stores lane). Every lane needs only `job-inputs`;
`ci-ok` and `required-checks` gate all of them, and
`scripts/ci/test_required_checks.py` runs both aggregators for every job,
result and selector combination.

Median over the runs below, queue wait excluded (job `started_at` to
`completed_at`; run wall clock is the first job start to `ci-ok` done):

| | Before | After |
|---|---|---|
| `check` job | 20.4 min (27 code runs, 08:20Z to 09:40Z on 2026-10-04) | 3.6 min |
| Longest job on the critical path | 20.4 min (`check`) | 9.4 min (`rust tests`) |
| Other lanes | | `ignored db (runtime)` 8.1 min, `ignored db (stores)` 5.8 min |
| Run wall clock | 27.6 min | 9.6 min |
| Docs-only run wall clock | about 2 min | about 1.5 min |

After: three runs of the split on one head (a pull-request run, its re-run,
and a manual dispatch): `rust tests` 9.4, 9.4 and 9.3 min; run wall clock
9.7, 9.6 and 9.5 min. The cargo cache key of the test lanes is the one the
old `check` job used, so those lanes started warm. The lint lane has a new
cache entry that main pushes will fill; its 3.1 to 4.0 min is a cold-cache
figure.

Probes on the split workflow: a docs-only change gets a green `ci-ok` with
the three test lanes skipped at the job level, and forcing a lane to fail
turns `ci-ok` and `required checks` red with the lane named in the log.

Costs and limits: total runner time per code run rises from about 20.4 to
about 27 min because each lane sets up its own container, service and
toolchain (hosted minutes for this public repo are free). The critical path
is now `rust tests`, where compiling and linking the integration binaries is
about 355 s; sharding those binaries is the next lever. Reconciling the
`REQUIRED` set in `test_required_checks.py` with the ruleset (`ci-ok`) is not
part of this change.
