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
a per-worktree subdirectory (`ws-<hash>`) under
`/paperclip/.cache/two-bot-next-bounded/slot-N/target`, and overrides inherited
`CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR`, including
values pointing to container `/tmp`. It sets `TMPDIR`, `TMP` and `TEMP` to the
same worktree's `slot-N/scratch/ws-<hash>`, for cooperative compiler/build/test
temporary files.
It disables incremental compilation and dev/test debuginfo **only in the child
environment**. It rejects
output/config overrides and manifests outside the current workspace. It neither
changes agent environments/rosters nor release profiles. `cargo fmt --all --
--check` does not compile and may run directly. Hosted CI and the image build keep
their existing Cargo commands on ephemeral runners, outside this controller pool.

There are **two independent leases**, not one global Cargo lock. Each lease gives
one Cargo invocation exclusive use of the slot. Cargo dep-info uses relative
paths and freshness is mtime-only, so two worktrees sharing one target
directory could reuse each other's artifacts (duplicate-crate `E0631`). Each
worktree therefore gets its own `ws-<hash>` subdirectory under `target/` and
`scratch/`. On acquire, with the slot lock held, other worktrees' subdirectories
are evicted to preserve the slot byte budget, and a legacy shared layout is
migrated after the same build-output-only check retention uses. Protected,
foreign, mixed, symlinked or unreadable contents refuse that slot unchanged
(the next slot is tried). Re-running in the same worktree keeps its warm
artifacts. A third concurrent invocation exits 75 immediately;
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

Each run also prints a `pool_provisioning_mismatch` JSON finding on stderr when
`min_available_bytes + slots * slot_budget_bytes` or `hard_limit_bytes` exceeds
the pool filesystem's total size. It is a warning only: admission continues
unchanged, and `retain` never emits it. It tells the Operator that the installed
policy assumes a larger backing filesystem than exists.

## Offline verification

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_pool_capacity_acceptance.py' -v
```

`test_pool_capacity_acceptance.py` pins the provisioning warning (including the
strict exact-fit boundary and `f_frsize` scaling) and the floor/receipt
behavior.

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
(deletes `slot-N/target` and `slot-N/scratch`, recreates both with their original
metadata, then unlinks `slot-N/lease.json`) while holding the SAME slot `lock` flock fd/inode from
re-verify through deletion and recreation. It never releases-then-deletes,
never replaces the lock file, and never touches `policy.json`, budgets,
quotas, registries, services, or `/tmp`.

Supply a fresh, complete, **host-scope** slot inventory (distinct `slots` key,
not `workspaces`). The sample lists only `slot-1`. A policy slot with no row
(here `slot-0`) is unattributed: it is reported ineligible and skipped with its
lease kept. The generic CLI still read-only inspects every policy slot, so a
receipt can show `slot-0` as skipped even though it was never a candidate.

```json
{
  "version": 1,
  "complete": true,
  "process_scope": "host",
  "captured_at_unix": 1790737200,
  "slots": [
    {
      "path": "/paperclip/.cache/two-bot-next-bounded/slot-1",
      "issue_id": "actual-issue-uuid",
      "status": "blocked",
      "idle_stale": true,
      "live_run": false,
      "referenced": false,
      "target_provenance": "build_output_only"
    }
  ]
}
```

This example is not a usable inventory; every value comes from a fresh
control-plane export.

Row semantics (all independently assessed, none inferred by the tool):

- **Status gate.** A terminal status passes. A nonterminal row (including
  `blocked`, `in_progress` or `in_review`) passes this gate only with
  `idle_stale` set to the JSON boolean `true`. Absent or `false` does not open
  the gate; a present string, number or null refuses the whole run.
  `idle_stale` records an independent Operator assessment, not a tool inference
  or elapsed-time heuristic. Keep the true issue status; never make an active
  checkout terminal. It opens **only** the status gate, not any other veto.
- **`live_run`** covers running, queued and retry runs on **all related**
  issues.
- **`referenced`** covers every execution, project and shared-workspace
  reference to that **exact slot**, including namespace/mount aliases. A mere
  reference to the active **source checkout** is not a reference to the cache
  slot. Any actual exact-slot reference still vetoes, even on a terminal card.
- **`target_provenance: build_output_only`** is one existing field that attests
  **both** the exact slot's `target` **and** `scratch` independently; each is
  also checked by the tool's own veto heuristics. Unclassified, mixed,
  unknown or missing stays ineligible. There is no separate schema field for
  scratch.

`complete: true` only after accounting for the whole relevant control-plane
set. The inventory must be at most 60 seconds old both before AND after the
scan; missing/ambiguous rows fail closed.

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
  --evidence /EXTERNAL-EVIDENCE/new-two-pool-retain-receipt.json
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

### Idle-stale over-budget exact-slot retention lifecycle

This lifecycle applies only to independently assessed **idle-stale,
over-budget build output**, retaining its true issue status. It does not
extend retention to active or under-budget caches. The generic command does
not enforce the over-budget scope: the Operator must measure whole-slot
allocated bytes against the unchanged policy and omit every other candidate.
An old incident receipt or a free lock is context, never eligibility.

The assessment records the current lease holder identities and dead/ended
wrapper, Cargo process-group and control-plane run evidence; complete absence
of running/queued/retry work and exact-slot references; host/container namespace
and device/inode reconciliation; and independent target **and** scratch output
classification. Age alone establishes none of these. Preserve source, active
and shared workspaces, evidence, backups and incident archives: they are never
cache candidates. Any doubt before execution means STOP, no mutation handoff.

1. **What `retain` does.** For an eligible slot it deletes `target` and
   `scratch`, recreates them empty, and unlinks `lease.json` only after that
   succeeds. There is no target-only mode, no dry-run and no lease-preserving
   mode. If the situation needs the lease left in place, STOP: this command is
   not the right tool.
2. **Preserve evidence before any mutation.** The CLI does not copy lease
   bytes. The Operator independently archives, outside the pool, the original
   `lease.json` raw bytes and their hash, plus the relevant transcript and
   diagnostics in a persistent external evidence location (not disposable run
   scratch). Verify the copy byte-for-byte and record its hash before proceeding.
   The transcript captures the invocation, stdout/stderr and exit status. If any
   evidence write fails, STOP. A preservation veto from the
   tool remains a veto. If output data must be kept, preserve it under a
   separately reviewed, bounded procedure and then restart the whole
   assessment from a fresh inventory.
3. **Record identities.** Record the exact slot identities (path,
   `(st_dev, st_ino)` of the slot, `lock`, `target`, `scratch`, owner uid/gid
   and permission modes) and the allocated bytes before and after.
4. **Verify the release conditions** before handing the slot back: that the
   recreated metadata is valid and writable by the build user; that the
   existing project quota (project id 9380) is inherited by the recreated
   directories; and that the hard quota is in force. Do not rely on a late,
   unlocked `chmod`/`chown` or on policy or quota edits to repair a slot.
   `retain` checks both roots for known owner/group restoration impossibilities
   before deleting either, under the same original lock. Ordinary non-root
   callers must own both roots and either belong to each recorded group or
   inherit it from a setgid parent; non-member inheritance cannot restore a
   setgid mode. This preflight is not a guarantee against later syscall failures.
   The command restores valid uid/gid and full permission modes under that lock.
   If restoration fails it keeps the lease and reports the partial mutation.
   The kept lease alone does not fence admission: maintain the independent hold
   through post-verification and STOP for separately authorized recovery.
5. **Evidence receipt.** Use `--evidence` with a **new**, external file; the CLI
   refuses an existing path rather than overwriting evidence. It creates and
   fsyncs a prepared record and its parent before invoking `retain`; this is
   not the lease archive or transcript. The final receipt write can
   still fail after mutation. On final write/flush/fsync failure the CLI emits
   the actual structured per-slot result to stderr before refusing with exit 75;
   capture it in the persistent transcript. A missing or incomplete receipt
   therefore never proves that nothing was mutated: compare the recorded
   identities and allocated bytes.

#### Bounded TWO-only build/dispatch admission hold

Existing supported controls only: slot `lock` flocks, `timeout(1)`,
read-only `policy.json`, and a fresh inventory. No service stop, no mass
cancellation, no environment/roster change, no policy or quota edit. Running
workers keep their slots: any holder uses non-blocking locks and never steals
or kills.

1. Start with a bounded **TWO-only** admission/dispatch hold, with its exact
   mechanism, scope, expiry and undo recorded. It must prevent new TWO starts
   through assessment, mutation **and post-verification** without changing
   services, environments or other projects. If an existing supported fence
   covering that whole interval cannot be demonstrated, STOP and request a
   separately reviewed hold procedure; do not improvise one. Record pre-hold
   `sha256sum policy.json`, lock inodes (`stat`), lease hashes and external
   evidence copies. The source checkout and omitted `slot-0` stay preserved.
2. A refusal of `run` admission may serve as hold evidence only if it was
   freshly proven and every new-run and race assumption still holds for the
   whole window (for example, no idle below-budget slot can appear). Observing
   that a lock is free is **not** sufficient evidence of a hold.
3. Otherwise start a bounded holder (for example `timeout 300`) that
   non-blocking flocks each *free* slot lock and sleeps. Locks held by running
   workers fail `LOCK_NB` and are reported as preserved, never touched.
4. Validate scope and quiescence, then complete the inventory. Export it and
   require the target row to satisfy the status gate above, with
   `live_run: false` and `referenced: false`. Do not stretch a preparation hold
   across asynchronous review: record its release/expiry, then establish a fresh
   bounded execution hold. **Re-export** after review and after any hold expiry. A
   complete inventory (at most 60 seconds old) must exist both before and after
   the process scan. The scan runs on the host namespace and covers processes
   plus alias device/inode identity.
5. If anything is doubtful (stale or expired hold, incomplete scan, unexplained
   reference, changed identity), do not hand off for mutation.
6. **Handoff** to run `retain` is Operator-only and needs a separate
   authorization, given only after the reviewed source is merged. Do not run
   it from an unmerged or unreviewed copy.
7. Under that independent admission fence, release only the temporary flock
   holder and run `retain` immediately in the same shell with the fresh
   inventory and an evidence path. The CLI needs the **original** lock inode;
   never unlink, replace or manually clear the lock/lease. A concurrent holder
   skips the slot, and stale inventory refuses, but neither makes the
   release-to-acquire gap atomic. If any dispatcher can start work in that gap
   or before post-verification, STOP: slot flocks alone cannot provide this
   continuous fence. Reassess from fresh evidence after any expiry.
8. Before releasing admission, retain the post-run control-plane/process scan
   and receipt, verify the release conditions above (build-user writability,
   inherited project 9380 and hard quota), and compare policy hash, original
   lock identity, exact paths and allocated bytes. Source and `slot-0` must be
   unchanged. Failed restoration or verification means STOP and a separately
   authorized recovery, never a late unlocked metadata repair.

If `retain` never ran, or refused before mutation, release the holder and
verify that the `policy.json` hash, lock inodes and lease files match the
pre-hold record, then re-probe admission. Do not read the absence of a receipt
as proof of that (see step 5 above): check the recorded identities. A completed
or partial mutation cannot be undone by this procedure. Deleted output is
regenerable and rebuilds only after the bounded pool is verified usable. The
archived lease is historical evidence, not an instruction to restore or unlink
a live sentinel manually. Source, secrets, services and archives are not changed
here. Code merge authorizes no host mutation; a fresh reviewed evidence pack
and separate normal Operator execution authorization are still required.

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

### Evidence-first removal handoff

This is a documentation/checklist contract, **not authorization for a live host
rehearsal or removal**. Code merge is not host deployment. Use the existing
Paperclip issue documents, native review stages and interactions; no new service,
cleanup tool, unattended deletion or waiver. DevOps owns future handoff
preparation, Engineering Lead independently reviews the evidence, and the
operator alone performs separately authorized host actions. Keep host details
and readiness packets in the restricted remediation card, not the public PR.

### Applicability before preparation

- Legacy `audit` covers **only immediate worktree `target/` candidates** under
  the genuine worktrees root. It is read-only, not a deletion procedure.
- Bounded-pool `retain` is a **separate lock-through-mutation path**. Reuse its
  exact-slot eligibility, evidence and same-lock controls above, including its
  bounded holder-to-`retain` transition; do not replace them with this checklist.
  With `--evidence`, the CLI exclusively creates and fsyncs a new external
  prepared record and its parent before invoking `retain`; the final result
  receipt is written afterwards and can still fail after mutation. Neither is
  a legacy pre-mutation audit export, and a missing or incomplete receipt does
  not prove no mutation occurred. Follow the pool lifecycle's external lease
  archive and transcript requirements as well. This legacy-removal contract
  adds no `retain` dry-run or new tool capability.
  A slot holder is not a general hold for legacy or external writers.
- Primary shared-workspace targets, old shared caches and arbitrary external
  targets are **unsupported by the legacy audit**. An empty audit, terminal
  cleanup card or absence of Cargo PIDs does not make them eligible. Preserve
  them pending a separately reviewed, separately authorized exact-path procedure.
  Never repoint `--worktrees`, fabricate eligibility booleans, widen tool scope
  or use `retain` on a path outside its supported slots.

### 1. PREPARE ONLY — retain a readiness packet

Do not rename, delete, clear a sentinel or otherwise mutate the target. Do not
bundle missing preparation with removal in an execution confirmation. Preparation
is not deletion authority. Missing capability or evidence means **STOP**: retain
the gap and no executable removal handoff.

Save a revisioned readiness packet as an issue document with retained attachments
where needed. All fields below are required; placeholders are not evidence:

| Packet field | Evidence to retain |
| --- | --- |
| Binding | Packet revision, handoff owner, preparation time, canonical exact target path and filesystem device/inode identity; container identity, aliases and canonical host/container mount or writable-layer mapping. |
| Executable and procedure | Reviewed **merged** executable and guard SHA, checkout/identity verification, exact applicable procedure/version and scope; explicit supported-candidate determination, not an empty-audit inference. |
| Independent provenance | Operator's independently established exact-target `build_output_only` classification and its source evidence; tool filename heuristics cannot mint it. Preserve mixed, unknown or unclassified material. |
| Control-plane references | Authoritative complete export and capture time covering running, queued and retry runs on all related issues, and every execution/project/shared workspace reference, including terminal cards; attribution source and results. |
| Host visibility | Host PID namespace and complete host/container cwd, exe, fd, mmap and cmdline visibility/results, alias/device/inode checks and denied/incomplete scan results. A container-only PID view is insufficient. |
| Bounded hold | Existing supported hold method, affected writers/admission scope, quiescence proof, owner, maximum duration, start/expiry, release/undo and verification. Prove it covers the exact procedure; no service stop, cancellation or environment/roster edit is implied. |
| Preflight output | Complete host inventory and applicable audit output, command/procedure identity, start/end times and exit status, plus allocated-byte baseline and protected-path list. Inventory must be at most 60 seconds old both before and after scanning. |
| Evidence retention | Durable packet/output locations and successful write/readback result, independent of the candidate; never store the only receipt inside a removal target. |

A preparation hold, if separately authorized and needed to obtain a readiness
sample, is bounded and released/allowed to expire before asynchronous review.
Record its release. **Do not keep admission held indefinitely for review.**
Expiry or release invalidates readiness for mutation; the packet remains review
evidence only. Existing fail-closed rollout/admission refusals are not a blanket
legacy-removal hold, and this checklist does not authorize changing them.

### 2. REVIEW — independent evidence gate, then one confirmation

Keep the executor assigned. Request Engineering Lead review of the exact packet
revision through a native `executionPolicy.stages` review stage; the executor
must never be its own reviewer. Preserve existing stage IDs when updating a
policy. Missing, unsupported or ambiguous evidence returns to preparation.

Only after that review passes may the handoff owner create **one**
operator-addressed `Operator step:` execution `request_confirmation` on the
existing handoff card. Bind its `payload.target` to the issue document's exact
revision and name the canonical exact target, procedure and merged guard SHA.
Use the operator's configured user identity, `resolverPolicy: human_only` and
`continuationPolicy: wake_assignee`; do not substitute a general board response.
No separate card is needed for each preflight substep.

Acceptance is **neither execution evidence nor a waiver**, and does not supply
missing normal authorization for host action. Never backfill a packet to imply
an unobserved preflight occurred. A changed target, procedure or packet revision
requires renewed independent review and a new exact-target confirmation; withdraw
or supersede the old request through the existing interaction flow.

### 3. EXECUTE — separate authorization and fresh final evidence

For legacy removal, the operator proceeds only with the separate normal
authorization, reviewed packet and exact-target confirmation. Earlier acceptance
cannot override any STOP below. Before **ANY mutation, including rename**:

1. Establish and record the supported **bounded hold** for this exact procedure:
   method, scope/quiescence, start, expiry and undo. Revalidate that it remains
   effective through mutation. A missing, expired or insufficiently scoped hold
   means skip deletion; do not improvise a new admission mechanism.
2. Revalidate the canonical exact path and device/inode identity, host/container
   mapping, reviewed merged executable/guard, procedure applicability and
   independent provenance. Recheck authoritative live/queued/retry and all
   execution/project/shared references and complete host process visibility.
3. **Re-export the complete host inventory and rerun the applicable audit
   immediately before removal**, under that hold. Preserve the **<=60-second
   inventory age both before and after scanning**. Old review output or an
   accepted confirmation cannot substitute. If evidence retention or other delay
   makes readiness stale, repeat the export/scan before considering mutation.
4. Retain and verify the **final evidence BEFORE mutation**: exact-target binding,
   hold record, fresh inventory, complete scan/audit output and exit status,
   provenance/references and allocated bytes before. This ordering is required
   for the separately authorized legacy procedure; neither `retain`'s prepared
   record nor its final result receipt supplies that legacy preflight evidence.
   Pool retention follows its separate controls above; never run legacy `audit`
   as a substitute or release its mutation lock.
5. Only if every gate still passes, use the separately authorized exact-path
   procedure. Missing/expired hold, stale inventory, denied/incomplete scan,
   positive reference, unsupported path or **failed final evidence write** means
   **STOP/skip deletion**, not rename first or confirm away the failure.

Retain a completion receipt for every attempt: packet revision, confirmation and
authorization references, actual outcome (removed, skipped, aborted or partial),
exact paths, allocated bytes before/after and reclaimed difference, hold release
or expiry and undo verification, and protected-path preservation results. Do not
claim reclaimed bytes from acceptance, estimates or an absent receipt. Preserve
source, evidence, entire workspaces, tracked files, active/shared caches, backups,
incident-preservation archives, Docker images and services. No sentinel clearing
is authorized by this legacy handoff; `retain` keeps its existing exact-slot rules.

### Fictional tabletop — no live rehearsal

These are checklist expectations, not host measurements or reconstructed incident
evidence. For each row, assume an earlier confirmation was already accepted:

| Fictional final-preflight condition | Required result |
| --- | --- |
| Inventory missing, or 61 seconds old before or after scanning | **STOP**; no executable removal handoff; obtain a new complete export and scan. |
| Hold absent, expired, released or not covering the target's writers | **STOP**; no executable removal handoff; establish a supported new bounded hold and redo final preflight. |
| Host process scan denied/incomplete, or only container PIDs visible | **STOP**; no executable removal handoff; preserve target until complete visibility is available. |
| Live, queued, retry, execution/project or shared reference found | **STOP**; no executable removal handoff; preserve the referenced target. |
| Provenance unclassified, mixed, unknown or derived only from filenames | **STOP**; no executable removal handoff; independent classification is required. |
| Primary shared-workspace or arbitrary external target; audit is empty | **STOP**; no executable removal handoff; preserve pending a separately reviewed authorized exact-path procedure. |
| Final evidence write/readback fails | **STOP**; no executable removal handoff or mutation, even with clean scan output in memory. |
| Target identity, procedure or packet revision differs from reviewed/confirmed binding | **STOP**; no executable removal handoff; renew review and exact-target confirmation. |

Fully supported fictional case: `/HOST/EXAMPLE/worktrees/closed-fixture/target`
is an immediate ignored, untracked, nonsymlink worktree target with independently
attested build-output-only provenance. Complete authoritative rows show terminal
attribution, no live/queued/retry runs and no execution/project/shared references;
complete host/container process checks are negative. DevOps retains packet R1,
including mapped identity, merged guard SHA, applicable approved procedure,
allocated-byte baseline, bounded hold plan and a fresh preparation audit. The
sample hold is released before Engineering Lead independently reviews R1 through
the native stage. Only then is one operator-addressed confirmation for R1 and
that exact target accepted; separate normal execution authorization is recorded.

Review finishes minutes later: **R1's sample is now stale and its hold released**.
The acceptance cannot revive readiness. With the binding unchanged, the operator
records a new supported bounded hold and exports a new inventory, scanned at age
8 seconds before and 12 seconds after scanning. The fresh final preflight and
successful durable write/readback are retained as execution evidence linked to
R1 **before any mutation**. Final evidence is an appended execution receipt, not
an edit to the reviewed packet; a material change instead goes back to review
and confirmation. While the hold remains effective, only the separately
authorized exact target is removed. The fictional completion receipt records
4,096 allocated bytes before, 0 after, actual removal, hold release/undo checks
and unchanged protected paths. No commands or host actions are run for this
example; it is not evidence of a real removal.

### Existing bounded-cache/monitor rollout and rollback

The evidence gates above precede any separately authorized removal in this
rollout; they do not expand provisioning, generator-guard or slot-recovery scope.
Keep the existing rollout receipts on the same remediation/handoff card.

1. Record `findmnt -T /home`, `findmnt -T <host TWO worktrees>`, `df -B1` and
   `statvfs` user-available bytes and the explicit container-to-host mapping.
   Do not trust the container's `/home` reading.
2. Before scratch checks or pool creation, establish and record the supported
   **bounded TWO-only build-admission restriction**: method, scope, start, expiry
   and release/undo. Keep it effective through pool provisioning, policy write
   and post-verification in steps 2–4; release it only in step 5. If it expires or
   loses coverage, **stop rollout**; re-establish and verify the supported bounded
   restriction before resuming. Do not extend it indefinitely for review.
   Resolve and retain the container-scratch coverage evidence above. If mapping,
   attribution, process visibility or external growth is unresolved, **stop rollout
   and preserve fail-closed admission refusal**; successful worktree audit alone
   is insufficient. This rollout restriction is distinct from the legacy-removal
   mutation hold. Release temporary **legacy-removal readiness holders** at their
   recorded bound before asynchronous evidence review; that release must not
   release the rollout restriction or stretch a legacy mutation hold across review.
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
3. After that evidence exists, write the pool policy:
   ```json
   {"version":1,"slots":2,"slot_budget_bytes":6442450944,
    "hard_limit_bytes":17179869184,"min_available_bytes":10737418240,
    "quota_receipt":"actual retained Operator quota evidence reference",
    "scratch_coverage_receipt":"actual retained Operator scratch coverage evidence reference"}
   ```
   The policy/slot count is fixed while any lease exists. It must not be writable
   by unrelated projects. Expose the new path to TWO containers using the existing
   `/paperclip` bind mount; do not change the agent roster or inherited environment.
4. Pin the monitor script at the merged SHA in the existing host monitoring
   mechanism; test both quiet healthy output and a synthetic threshold finding by
   setting the threshold above current available bytes. Register the existing alert
   route. Adopt the repo wrapper for subsequent TWO builds; no full Rust build is
   needed for this rollout. Record slot usage, quota enforcement and zero new
   per-worktree targets for the first concurrent isolated invocations.
5. Release the rollout admission restriction only after the guard, quota, monitor **and scratch
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
