# Controller Cargo cache: bounded admission and safe retention

Incident: on 2026-09-30 the controller's **/home**, not its root filesystem,
exhausted user-available bytes. Per-worktree Rust output accumulated even on
completed cards. A completed card can still have a live workspace reference.
This procedure is TWO Bot Next only; it does not touch services or databases.

## Local builds

Use the repository wrapper from your isolated workspace:

```sh
python3 scripts/cargo_cache.py run -- check -p two-bot-core
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib
```

Run the wrapper with a Bash timeout that covers the build (600000 ms) or in
the background. An interrupted run keeps its `lease.json` sentinel and any
partial output stays in the slot, counting against its budget; a stale
under-budget lease with a dead recorded Cargo process group is recovered on
the next acquire, while over-budget or live-group leases stay for Operator
inspection.

Choose the smallest useful target. The wrapper adds `--offline --locked`, uses
`/paperclip/.cache/two-bot-next-bounded/slot-N/target`, and overrides inherited
`CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR`, including
values pointing to container `/tmp`. It sets `TMPDIR`, `TMP` and `TEMP` to the
same lease's `slot-N/scratch`, for cooperative compiler/build/test temporary files.
It disables incremental compilation and dev/test debuginfo **only in the child
environment**. It rejects
output/config overrides and manifests outside the current workspace. It neither
changes agent environments/rosters nor release profiles. `cargo fmt --all --
--check` does not compile and may run directly. Hosted CI and the image build keep
their existing Cargo commands on ephemeral runners, outside this controller pool.

There are **two independent leases**, not one global Cargo lock. Each lease gives
one Cargo invocation exclusive use of a stable target directory, preserving
artifacts for later worktrees. A third concurrent invocation exits 75 immediately;
it does not queue indefinitely, create another target, or fall back to a local
`target/`. Retain/continue the card rather than evading the limit. A missing pool,
quota receipt, scratch-coverage receipt, low filesystem headroom, bad policy,
over-budget target/scratch, missing scratch directory, or crash sentinel also
refuses admission. Do not run direct compiling Cargo commands on the
controller, including when the pool is unavailable.

The wrapper samples allocated blocks for the **whole slot**, including retained
scratch and lease metadata, and `f_bavail * f_frsize` every second. Scratch is
retained under the same lease/quota; it is not automatically cleaned. A
budget/floor breach terminates its own process group and leaves `lease.json` for
inspection. A stale under-budget sentinel is recovered automatically: holding the
slot flock proves no wrapper, Cargo, or fd-inheriting descendant is alive, and a
dead recorded Cargo process group confirms it; recovery emits a
`stale_lease_recovered` line and unlinks the sentinel. Over-budget sentinels,
live recorded groups, and unparseable or pgid-less leases (including ones minted
before pgid recording) are **never stolen** — the Operator inspects those.
Cargo inherits the lease FD too. Normal completed Cargo
invocations release the lease, including commands returning a compile/test error.
An outliving process group leaves the sentinel. Do not clear a sentinel without
both control-plane and actual-process checks.

The wrapper resolves Cargo **before** touching the pool: `PATH`, then
`$CARGO_HOME/bin/cargo`, then `~/.cargo/bin/cargo` (agent PATH may lack rustup's
bin directory). If none exists it exits 75 without a lease. A fallback-resolved
directory is prepended to the child's `PATH` only, so rustup's sibling proxies
resolve too. If the spawn itself fails (`Popen` raises `OSError`: exec/fork
failed, so no writer ever existed), the wrapper still holds the slot flock and
removes its **own** `lease.json` before exiting 75. Before this fix such runs
left sentinels in both slots (TOG-11995). Signal, budget/floor and
outliving-group paths still retain the sentinel. Build cancellation is idempotent
from the first signal onward: the first SIGINT/SIGTERM raises out of the poll
loop, and every later signal is a no-op, so cleanup always reaches the
SIGTERM/SIGKILL process-group stop and a second signal can never abandon a live
writer before SIGKILL.

### What is and is not bounded

- Fixed slot count bounds concurrent target trees, not their byte size by itself.
- A **host-enforced filesystem quota on the new pool** is the hard aggregate byte
  bound. Recommended initial policy: two 6 GiB sampled slot budgets, 16 GiB hard
  aggregate quota, 10 GiB minimum available bytes. Four GiB is aggregate sampling
  and metadata headroom, **not a guaranteed overshoot limit**.
- A sampled userspace check cannot enforce a hard byte ceiling during a burst.
  `policy.json` requires an Operator quota receipt, but this is an attestation;
  the Python tool does not create, inspect, or verify kernel quotas. Do not claim
  deployment or a hard bound without independent quota evidence.
- Both `target` and `scratch` must inherit the **same pool quota**; independent
  slot trees permit two builds, not separate unbounded compiler temp areas.
- Existing legacy targets (including external `CARGO_TARGET_DIR` in container
  `/tmp`), the old shared `cargo-target-two-bot-next`, Cargo's registry, other
  repos, backups and archives are **not** included in this quota. No automatic
  deletion or migration of these is implemented. Offline admission prevents the
  wrapper from downloading more registry content.
- The wrapper is a cooperative repository build entry point, not a sandbox for
  hostile build scripts/tests and not a transparent Cargo intercept. Programs
  hardcoding `/tmp`, custom compiler wrappers, Cargo config, or direct Cargo can
  still write outside the pool. Rollout is incomplete until TWO callers use it,
  external paths are accounted for, and new legacy/scratch growth is checked.

### Container scratch: unresolved, fail-closed rollout gate

Operator evidence at 2026-09-30 03:38Z identifies Docker writable-layer data on
host `/home`; a container's `/tmp` is **not spare root-filesystem capacity**.
The Operator independently measured these allocated bytes; this agent did not
remeasure them or remove anything:

| Container path | Operator-measured bytes |
| --- | ---: |
| `/tmp/tog-10078-fixes-target` | 4,654,657,536 |
| `/tmp/tog-10078-db-target` | 1,527,181,312 |
| `/tmp/two-bot-next-s2` | 2,225,819,648 |

The three paths total **8,407,658,496 bytes**. Peer reports of approximately 19G
container `/tmp` and a 20.6G writable layer are **not independently verified**;
do not add layer totals to their component paths or claim them as reclaimed.
The host recovery receipt `/tmp/operator-20260930T033153Z-cache-reclaimed.json`
records a separate worktree cleanup, not removal of these scratch paths.

**The worktree-only retention audit below does not cover `/tmp` or arbitrary
external targets. Its success must not clear this gate.** Do not extend candidate
selection by globbing `/tmp/*target*`, a directory name, or issue terminal status.
Scratch can contain source/evidence, active runs, shared data or mixed outputs.

Keep the TWO build-admission hold until the Operator records a scratch-coverage
receipt on the **same rollout card** with:

1. Canonical container-to-host mount/writable-layer mapping, measured backing
   filesystem and allocated-byte accounting for these paths and all discovered
   TWO Cargo target/scratch overrides. Record container identity, path aliases,
   issue/workspace attribution, and live/queued/retry/shared references. Unknown
   attribution or inaccessible paths mean **unresolved**, not disposable.
2. Root-visible host/container cwd/exe/fd/mmap/**cmdline** checks with no unreadable
   process directories, accounting for mount namespace aliases. Preserve every
   live/shared/unattributed path and all source/evidence/archives. This script
   neither supplies external-path eligibility nor authorizes scratch deletion;
   any proposed external cleanup needs a separately validated exact-path procedure
   within the existing Operator handoff. No generic `/tmp` cleanup is permitted.
3. Adoption evidence that inherited external Cargo output and cooperative temp
   output go to the new pool; prove quota inheritance for **target and scratch**.
   Tiny fake invocations from two isolated workspaces suffice; no full Rust build.
   Record before/after external-target and writable-layer growth observations.
   Existing preserved output must be explicitly accounted for within host headroom;
   unexplained growth or a writer ignoring the wrapper/temp settings leaves this
   gate unresolved and goes to the Director of Engineering.

Only after this evidence exists may the Operator set `scratch_coverage_receipt`
in `policy.json` and release admission. The wrapper **refuses launch** when that
receipt is absent/blank, even with a quota receipt. Like `quota_receipt`, this is
an attestation pointer, not code verification of host mappings or receipt contents.
Until then rollout remains **not deployed/unresolved**; do not fill the field with
a placeholder. No environment/roster changes, mount/service operations, archive
pruning or new spend are authorized by this gate.

## Offline verification

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
```

The suite launches fake compilers that write 4 KiB to target and 4 KiB to scratch
each and block on tiny fixture markers. It proves two isolated invocations run
simultaneously in different slots, a third is refused, whole-slot allocated bytes
stay below the fixture budgets, oversized target **and scratch** writers are
stopped, and signals leave a crash sentinel. It also tests inherited external
Cargo/temp/**repository-scratch** overrides
(`PAPERCLIP_RUN_SCRATCH_DIR`/`PAPERCLIP_SCRATCH_DIR`, which `mac.rs` tests
prefer over `temp_dir`), real `tempfile` placement, unchanged parent
environment, repeated signals during shutdown plus a deterministic second-signal
transition fixture, stale-lease recovery on a free lock with a dead recorded
group (held locks, over-budget leases and live groups stay refused),
missing/symlink scratch refusal,
retained-scratch admission limits, missing scratch-coverage attestation, a
missing Cargo refused before leasing, `$CARGO_HOME`/`~/.cargo` resolution, a
failed spawn (mocked and real exec failure) releasing its own lease, and
preservation of the three external path fixtures. Retention tests
include an **actual Linux child process with an open FD and mmap**, plus container
path aliases checked by device/inode identity, attested Operator
`target_provenance` gating with tiny unclassified/mixed `.json`/archive/source
fixtures under allowed subtrees alongside build-output-only preservation
vetoes for material the heuristics do catch, and refusal of noncanonical,
symlink-spelled, or device/inode-aliased inventory rows. All fixtures live in the run scratch
folder when `PAPERCLIP_RUN_SCRATCH_DIR` is set. No Rust build, multi-GiB fixture,
network, Discord or production/staging store is required. These tests do **not**
measure real Rust peak size or prove a production quota. The Operator must record
quota state and a tiny saturation/refusal check before rollout.

## Read-only legacy retention audit

Only immediate `<worktrees>/<workspace>/target` directories are candidates.
External targets and container scratch are **not audited** and remain covered by
the unresolved rollout gate above; no output here is a complete storage inventory.
Supply
a fresh, complete, **host-scope** control-plane inventory:

```json
{
  "version": 1,
  "complete": true,
  "process_scope": "host",
  "captured_at_unix": 1790737200,
  "workspaces": [
    {
      "path": "/HOST/PATH/two-bot-next/.paperclip/worktrees/terminal-card",
      "issue_id": "actual-issue-uuid",
      "status": "done",
      "live_run": false,
      "referenced": false,
      "target_provenance": "build_output_only"
    }
  ]
}
```

This example is not a usable inventory. The Operator exports current authoritative
issue/run/workspace rows: `live_run` must include **running, queued and retry**
runs, on any related issue; `referenced` must include **all execution/project/shared
workspace references**, including those from terminal cards. One row per canonical
host workspace path; ambiguous/missing attribution is not permission to prune.
Mark `complete` only after accounting for the whole relevant control-plane set.
Do not infer these booleans from terminal status, a PID lookup, or a directory name.
The tool intentionally does not guess an API/storage schema or obtain credentials.

`target_provenance` is an independently recorded, exact-target Operator
classification: `build_output_only` means the Operator has verified this exact
target directory holds only regenerable Cargo output; `unclassified`, `mixed`,
or `unknown` (or a missing field) keeps the candidate ineligible. Filename
heuristics inside the tool cannot establish this — evidence, archives, or
sources stashed under Cargo's own subtrees (e.g. `debug/incident-20260930.json`)
pass every name check — so the classification is attested control-plane data,
never minted from the tool's own heuristics (which remain only as a backstop
veto). Inventory paths must be absolute canonical host spellings: symlink
spellings are refused outright, and rows whose existing workspaces share a
`(st_dev, st_ino)` identity with another row refuse the whole audit, so a
conflicting live row under an alias path cannot be ignored. Inaccessible
workspace paths refuse the audit rather than being silently skipped.

Run on the **host in its PID namespace**, with read access to *all* process cwd,
exe, fd and map entries, including Docker/container processes. A container's own
`/proc` is insufficient. Inodes are compared too, so container mount path spellings
need not match the host. Any denied/incomplete process or workspace scan aborts the
entire audit. Kernel threads/zombies carry no userspace references and are skipped.
The inventory must be at most 60 seconds old both before and after scanning.

```sh
python3 scripts/cargo_cache.py audit \
  --worktrees /HOST/PATH/two-bot-next/.paperclip/worktrees \
  --inventory /RUN-SCRATCH/two-cache-inventory.json
```

Output is JSON with `audit_only: true`; **the tool never deletes anything**.
An eligible candidate needs terminal issue attribution, no live run/reference,
an attested Operator `target_provenance: build_output_only` classification on
its exact-target inventory row, zero tracked target files, a Git-ignored target,
no symlink, no actual process reference anywhere in its workspace
(source/evidence included), and **build-output-only target contents**: only
Cargo's own top-level entries
(`debug`, `release`, `doc`, `package`, `tmp`, `.rustc_info.json`, `.cargo-lock`,
`CACHEDIR.TAG`), with nested build-script codegen (`debug`/`release`
`build/*/out/*.rs`) expected. A `.gitignore` entry
proves nothing about provenance, so preserved evidence/backups/archives/sources
or any other foreign entry inside `target/` vetoes eligibility (fail closed),
and foreign files under Cargo's own subtrees that pass every name check stay
ineligible without the Operator classification. Inventory paths must be
absolute, canonical, and unique: noncanonical spellings
(`/worktrees/./a`), symlink spellings, or duplicate filesystem
(`st_dev`, `st_ino`) identities with conflicting rows refuse the whole
audit. A complete control-
plane snapshot and a proc scan are still not atomic with future dispatch. Therefore
an audit receipt is not deletion authority. For any approved deletion, the Operator
must hold TWO build/dispatch admission, re-export and re-audit immediately before
removing the **exact target path only**, and retain an evidence receipt. If admission
cannot be held or scans cannot be complete, **skip deletion**. Never turn this
command into unattended cron cleanup. Never prune the shared cache through this
legacy-worktree audit.

## Shared-pool lock-through-mutation retention (`retain`)

The legacy `audit` above covers worktree targets only and **must never prune
the shared pool**. Over-budget shared slots with dead recorded holders need a
separate, reviewed retention path that holds exclusivity through the mutation
itself: `scripts/cargo_cache.py retain`. It mutates **exact slot paths only**
(`slot-N/target`, `slot-N/scratch`, `slot-N/lease.json`, then recreates empty
`target`+`scratch`) while holding the SAME slot `lock` flock fd/inode from
re-verify through deletion and recreation. It never releases-then-deletes,
never replaces the lock file, and never touches `policy.json`, budgets,
quotas, registries, services, or `/tmp`.

Supply a fresh, complete, **host-scope** slot inventory (distinct `slots` key,
not `workspaces`):

```json
{
  "version": 1,
  "complete": true,
  "process_scope": "host",
  "captured_at_unix": 1790737200,
  "slots": [
    {
      "path": "/paperclip/.cache/two-bot-next-bounded/slot-0",
      "issue_id": "actual-issue-uuid",
      "status": "done",
      "live_run": false,
      "referenced": false,
      "target_provenance": "build_output_only"
    }
  ]
}
```

`live_run` covers **running, queued and retry** runs; `referenced` covers
**all execution/project/shared workspace refs** to that exact slot.
`target_provenance: build_output_only` is an independently recorded,
exact-slot Operator classification (unclassified/mixed/unknown or missing
stays ineligible). `complete: true` only after accounting for the whole
relevant control-plane set. The inventory must be at most 60 seconds old
both before AND after the scan; missing/ambiguous rows fail closed.

Run on the **host in its PID namespace** with read access to *all* process
cwd, exe, fd, mmap **and cmdline** entries, including container processes.
Device/inode identity is compared too, so container spellings need not match
the host; lexical-only cmdline entries still veto by path. Deleted
(unlinked/replaced) artifacts fail closed, and any denied/incomplete process
or slot scan aborts the whole run (unresolved deleted references refuse with
no mutation). Per-slot doubt (held lock, unexpected contents, unattributed
or live/referenced row, missing attestation, provenance veto in target or
scratch, actual process reference, replaced lock) skips that slot and keeps
its lease.

```sh
python3 scripts/cargo_cache.py retain \
  --pool /paperclip/.cache/two-bot-next-bounded \
  --inventory /RUN-SCRATCH/two-pool-inventory.json \
  --proc-root /proc \
  --evidence /RUN-SCRATCH/two-pool-retain-receipt.json
```

Output is JSON with `retain: true`, an `excluded_deleted_references` count,
and a per-slot record (`eligible`, `before_bytes`/`after_bytes`/
`reclaimed_bytes`, `lock_held_through_mutation`). The Operator holds TWO build/dispatch
admission, re-exports a fresh inventory immediately before running, and
retains the evidence receipt. Scratch has no Cargo-defined top-level names
(temp files are arbitrary), so only protected directory/file/suffix vetoes
apply there — but the attested classification is still required. Never turn
this command into unattended cron cleanup.

### Deleted-reference evidence model (multi-tenant host)

Per-slot checks veto on positive attribution only: a lexical path inside the
slot's `target`/`scratch`, or a `(device, inode)` identity in that slot's
output. The whole-run check refuses a deleted entry attributable to no held
slot, with two provable exceptions:

- **Different-filesystem exclusion.** One filesystem's unlinked inode can
  never be another filesystem's file, so a stat-backed deleted entry whose
  device appears in no held slot output is provably unable to reference slot
  output. fd/cwd/exe entries are stat-backed by construction. A file-backed
  maps entry is re-grounded through `/proc/PID/map_files/<range>` — a symlink
  to the mapped file itself whose fstat device/inode compare exactly like fd
  stat — and becomes stat-backed too. Such entries are excluded and counted
  in the receipt as `excluded_deleted_references` — never silently dropped.
  The maps range is normalised to the unpadded name the kernel uses (maps
  zero-pads it, for example `00400000-…`). Following a map_files link needs
  CAP_SYS_ADMIN or CAP_CHECKPOINT_RESTORE; a denied read is not a scan
  failure. Maps entries with no usable map_files stat (anonymous object,
  hidepid, exit/munmap race, or denied without the capability) keep the
  kernel-printed superblock device, which need not equal the stat device for
  the same file (btrfs per-subvolume anon_dev, pre-6.8 overlayfs), so a
  "foreign" maps device there proves nothing and stays fail-closed.
- **Non-file identity exclusion.** A deleted path that cannot be a regular
  file at all cannot alias slot output and is excluded by identity: SYSV IPC
  shared-memory segments (`/SYSV<key>`), `/dev/zero`, memfd anonymous RAM
  files (`/memfd:<name>`), async-IO contexts (`[aio]`, container-spelled
  `/[aio]`), `anon_inode:` objects, and bracketed anonymous kernel mappings
  (`[heap]`, `[stack]`, `[anon:…]`). Browsers, PostgreSQL backends, and
  language runtimes map these ubiquitously, so without this rule shared-host
  mappings refuse every run. The bracket rule only matches a path whose whole
  name past an optional leading slash is bracketed; slot outputs always carry
  absolute paths under slot target/scratch directories (checked by lexical
  attribution first), so no regular file can match.

Everything else stays fail-closed: same-filesystem unattributed entries (a
deleted slot file held open is indistinguishable from an unrelated
same-filesystem temp file), device-unknown entries, incomplete or denied
process scans (denied fd, cwd, exe, maps, or cmdline reads), and any run where
a held slot's output device is unreadable (then nothing is device-excluded).

Limitations: unrelated deleted files on the *same* filesystem as slot output
still refuse the whole run — as do maps deleted entries with no usable
map_files stat that attribute to no slot (including real tmpfs paths such as
`/dev/shm` files, whose non-aliasing cannot be proven without
mount-namespace analysis) — quiesce writers or supply an independently
verified exact-path process-reference receipt instead. Exclusion assumes no
filesystem topology change under held slots during the bounded run (all locks
are held throughout). The read-only legacy `audit` keeps the strict global
rule except for the same non-file identity exclusion; only `retain`
partitions by device.

### Bounded TWO-only build/dispatch admission hold (and undo)

Existing supported controls only: slot `lock` flocks, `timeout(1)`,
read-only `policy.json`, and a fresh inventory. No service stop, no mass
cancellation, no environment/roster change. Running workers keep their slots:
the holder below uses non-blocking locks and never steals or kills.

1. Record pre-hold state: `sha256sum policy.json`, lock inodes (`stat`), and
   lease hashes. If `run` admission already refuses (wedged pool, no idle
   below-budget slot), record that refusal as the hold evidence and skip to
   step 4 — no holder process is needed.
2. Otherwise start a bounded holder (e.g. `timeout 300`) that non-blocking
   flocks each *free* slot lock and sleeps; locks held by running workers
   fail `LOCK_NB` and are reported as preserved, never touched.
3. Validate scope and quiescence: the holder reports held vs worker-held
   slots; a re-probe shows no idle slot for new `run` admission. Export a
   fresh complete host-scope inventory (≤60s): every row terminal with
   `live_run: false` and `referenced: false`.
4. Release the holder (kill its PID; expiry is the `timeout`) and run `retain`
   immediately in the same shell with the fresh inventory and an evidence
   path. The residual release-to-acquire race stays fail-closed: a new writer
   holds its slot lock (slot skips, lease kept) and inventory revalidation
   refuses a stale run with no mutation.
5. Release/expiry: the holder always ends via kill or timeout; `retain` ends
   by exiting. Verify the post-run inventory and the evidence receipt.

Undo (retain never ran or aborted before mutation): release the holder,
verify `policy.json` hash, lock inodes and lease files match the pre-hold
record, and re-probe admission behavior. No receipt means no mutation
occurred; running workers were never touched. If `retain` mutated some slots
and then must be rolled back, regenerable output rebuilds through the
repaired bounded pool; source, secrets, services and archives have no
rollback change here.

## /home available-byte alarm

Install/run on the **host**, not inside an agent container:

```sh
python3 /PINNED/REPO/scripts/cargo_cache.py filesystem \
  --path /home --backing-path /HOST/PATH/two-bot-next/.paperclip/worktrees \
  --min-available-bytes 10737418240
```

Healthy output is silent (exit 0). A low available-byte count or mismatched backing
filesystem is a JSON finding (exit 1). Visibility/configuration failures report a
refusal (exit 75), also actionable. Forward only findings to the existing incident
channel with its existing deduplication; do not invent a new credential/webhook.
Recommend a 2-minute interval and one incident per finding until recovery. No
service restart is needed. It uses user-available bytes, **not** root-reserved free
bytes or a percent-full threshold. Monitoring `/` alone is not acceptable. In an
agent container `/home` may be overlay while `/paperclip` is the host bind mount;
this mismatch is a finding, not evidence that the host has recovered.

## Operator handoff and rollback (one host step)

Use one existing/new `Operator:` child of the remediation card for this entire
bounded-cache/retention/monitor rollout; do not file separate cards per substep.
Code merge is not host deployment. Execution requires the reviewed, merged SHA.

1. Record `findmnt -T /home`, `findmnt -T <host TWO worktrees>`, `df -B1` and
   `statvfs` user-available bytes. Resolve the container-to-host path mapping
   explicitly. Do not trust the container's `/home` reading.
2. Under a bounded TWO-only build admission hold, export the fresh inventory and
   run the audit in the host namespace. Use the incident's already-authorized
   target-only pruning procedure for explicitly approved candidates, with a new
   immediate inventory/proc recheck per deletion. Record exact paths and reclaimed
   allocated blocks. No source/evidence/worktree, tracked file, active/shared cache,
   backup, preservation archive, Docker image or service may be removed here.
3. Resolve and retain the container-scratch coverage evidence above. If mapping,
   attribution, process visibility or external growth is unresolved, **stop rollout
   and keep admission held**; successful worktree audit alone is insufficient.
   Create the **new, initially empty** pool and its two `slot-N/target` and
   `slot-N/scratch` directories plus `slot-N/lock` regular files. Do not repoint or
   migrate the old shared cache or any legacy `/tmp` output.
   Apply an existing-supported project/directory quota to the new pool **only**,
   hard limit 17179869184 bytes. Record a quota query proving path, project ID,
   inheritance and hard limit, plus a tiny disposable limit/refusal drill in a
   separate scratch project on the same quota mechanism. No multi-GiB writes.
   **If this filesystem lacks already-supported quotas, stop this substep and
   route the constraint to the Director of Engineering.** No global remount,
   new disk, package install or unbounded soft-only fallback is authorized here.
4. After that evidence exists, write the pool policy:
   ```json
   {"version":1,"slots":2,"slot_budget_bytes":6442450944,
    "hard_limit_bytes":17179869184,"min_available_bytes":10737418240,
    "quota_receipt":"actual retained Operator quota evidence reference",
    "scratch_coverage_receipt":"actual retained Operator scratch coverage evidence reference"}
   ```
   The policy/slot count is fixed while any lease exists. It must not be writable
   by unrelated projects. Expose the new path to TWO containers using the existing
   `/paperclip` bind mount; do not change the agent roster or inherited environment.
5. Pin the monitor script at the merged SHA in the existing host monitoring
   mechanism; test both quiet healthy output and a synthetic threshold finding by
   setting the threshold above current available bytes. Register the existing alert
   route. Adopt the repo wrapper for subsequent TWO builds; no full Rust build is
   needed for this rollout. Record slot usage, quota enforcement and zero new
   per-worktree targets for the first concurrent isolated invocations.
6. Release the admission hold only after the guard, quota, monitor **and scratch
   coverage** are verified. Attach quota/audit/monitor/adoption/scratch receipts to
   the same Operator card. Admission tests before final policy use disposable
   offline fixture pools; never put a synthetic receipt in the real pool.

Rollback: hold new TWO compilation, let active invocations finish (do not kill
unrelated processes), disable only the new monitor entry and pool admission policy.
Preserve pool targets and all evidence. Keep the quota in place until no lease or
actual process references remain. Do not restore direct per-worktree builds as a
silent fallback. Stop there and report to Engineering if a further host change is
needed. Deleted disposable build output can be regenerated through a repaired,
bounded pool; source, secrets, services and archives have no rollback change here.
