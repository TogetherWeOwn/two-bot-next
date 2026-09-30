# TWO Bot Next controller builds

On the persistent Paperclip controller, run compiling Cargo commands through
`python3 scripts/cargo_cache.py run -- <check|test|clippy|build> ...` from your own
isolated workspace. Pick the smallest test/package that proves the change.
The wrapper supplies offline/locked mode, lean dev/test settings and one of two
independent cache leases. See [the build-cache runbook](docs/build-cache.md).

Do not create a per-worktree `target/` or an external/container `/tmp` Cargo target,
point Cargo at the old unbounded shared cache, change agent environments/rosters,
or evade a busy/refused/missing pool by running Cargo directly. The wrapper places
cooperative temporary output in quota-covered lease scratch, not container `/tmp`.
A missing pool/quota/scratch-coverage receipt needs the Operator rollout;
a full pool needs safe retention/continuation, not another cache. Agents must not
clear crash sentinels, delete host caches, mount filesystems or restart services.

`cargo fmt --all -- --check` does not compile and can run directly. The cache
regressions are offline Python fixtures:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache.py' -v
```

Ephemeral hosted CI and Docker image builds keep their existing Cargo commands.
Do not use production/staging databases for tests; use only authorized test
containers/CI services. Source, evidence, active/shared workspaces, backups and
incident-preservation archives are never build-cache retention candidates.
