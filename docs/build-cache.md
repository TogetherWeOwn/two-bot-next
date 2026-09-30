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

Choose the smallest useful target. The wrapper adds `--offline --locked`, uses
`/paperclip/.cache/two-bot-next-bounded/slot-N/target`, disables incremental
compilation and dev/test debuginfo **only in the child environment**. It rejects
output/config overrides and manifests outside the current workspace. It neither
changes agent environments/rosters nor release profiles. `cargo fmt --all --
--check` does not compile and may run directly. Hosted CI and the image build keep
their existing Cargo commands on ephemeral runners, outside this controller pool.

There are **two independent leases**, not one global Cargo lock. Each lease gives
one Cargo invocation exclusive use of a stable target directory, preserving
artifacts for later worktrees. A third concurrent invocation exits 75 immediately;
it does not queue indefinitely, create another target, or fall back to a local
`target/`. Retain/continue the card rather than evading the limit. A missing pool,
quota receipt, low filesystem headroom, bad policy, over-budget target, or crash
sentinel also refuses admission. Do not run direct compiling Cargo commands on the
controller, including when the pool is unavailable.

The wrapper samples allocated blocks and `f_bavail * f_frsize` every second. A
budget/floor breach terminates its own process group and leaves `lease.json` for
inspection. A crashed wrapper's sentinel is **never stolen**, regardless of PID
reuse or issue status. Cargo inherits the lease FD too. Normal completed Cargo
invocations release the lease, including commands returning a compile/test error.
An outliving process group leaves the sentinel. Do not clear a sentinel without
both control-plane and actual-process checks.

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
- Existing legacy targets, the old shared `cargo-target-two-bot-next`, Cargo's
  registry, other repos, backups and archives are **not** included in this quota.
  No automatic deletion or migration of these is implemented. Offline admission
  prevents the wrapper from downloading more registry content.
- The wrapper is a cooperative repository build entry point, not a sandbox for
  hostile build scripts/tests and not a transparent Cargo intercept. Rollout is
  incomplete until TWO callers use it and new legacy-target growth is checked.

## Offline verification

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
```

The suite launches fake compilers that write 4 KiB each and block on tiny fixture
markers. It proves two isolated invocations run simultaneously in different slots,
a third is refused, retained allocated bytes stay below the fixture budgets, an
oversized writer is stopped, and signals leave a crash sentinel. Retention tests
include an **actual Linux child process with an open FD and mmap**, plus container
path aliases checked by device/inode identity. All fixtures live in the run scratch
folder when `PAPERCLIP_RUN_SCRATCH_DIR` is set. No Rust build, multi-GiB fixture,
network, Discord or production/staging store is required. These tests do **not**
measure real Rust peak size or prove a production quota. The Operator must record
quota state and a tiny saturation/refusal check before rollout.

## Read-only legacy retention audit

Only immediate `<worktrees>/<workspace>/target` directories are candidates. Supply
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
      "referenced": false
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
zero tracked target files, a Git-ignored target, no symlink, and no actual process
reference anywhere in its workspace (source/evidence included). A complete control-
plane snapshot and a proc scan are still not atomic with future dispatch. Therefore
an audit receipt is not deletion authority. For any approved deletion, the Operator
must hold TWO build/dispatch admission, re-export and re-audit immediately before
removing the **exact target path only**, and retain an evidence receipt. If admission
cannot be held or scans cannot be complete, **skip deletion**. Never turn this
command into unattended cron cleanup. Never prune the shared cache through this
legacy-worktree audit.

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
3. Create the **new, initially empty** pool and its two `slot-N/target` directories
   plus `slot-N/lock` regular files. Do not repoint or migrate the old shared cache.
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
    "quota_receipt":"actual retained Operator evidence reference"}
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
6. Release the admission hold only after the guard, quota and monitor are verified.
   Attach quota/audit/monitor/adoption receipts to the same Operator card.

Rollback: hold new TWO compilation, let active invocations finish (do not kill
unrelated processes), disable only the new monitor entry and pool admission policy.
Preserve pool targets and all evidence. Keep the quota in place until no lease or
actual process references remain. Do not restore direct per-worktree builds as a
silent fallback. Stop there and report to Engineering if a further host change is
needed. Deleted disposable build output can be regenerated through a repaired,
bounded pool; source, secrets, services and archives have no rollback change here.
