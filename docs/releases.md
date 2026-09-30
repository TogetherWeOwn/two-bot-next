# Release architecture

The container has one release version, one root changelog, and one flat
`vX.Y.Z` tag. Internal crates are not released separately.

## Why there is a root library

The root manifest originally defined only a virtual Cargo workspace.
release-please's native Rust strategy requires a root `[package]` version;
adding a targetless package is invalid Cargo, even with `default-members`.
The root now has a minimal, non-published release-metadata library at
`src/lib.rs`. Explicit `default-members` preserve the previous selection of
all four application crates and also include the metadata library.

Cargo documents [root packages](https://doc.rust-lang.org/cargo/reference/workspaces.html#root-package)
and the [default library target](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#library).

## Pinned native strategy

The release action is pinned to
[`45996ed1f6d02564a971a2fa1b5860e934307cf7`](https://github.com/googleapis/release-please-action/tree/45996ed1f6d02564a971a2fa1b5860e934307cf7)
(v5.0.0), whose lockfile and bundled implementation use release-please
17.6.0. The single root `rust` strategy synchronizes the root package,
member versions, local dependency requirements, and root lockfile:

- [Rust strategy](https://github.com/googleapis/release-please/blob/v17.6.0/src/strategies/rust.ts)
- [Cargo manifest updater](https://github.com/googleapis/release-please/blob/v17.6.0/src/updaters/rust/cargo-toml.ts)
- [Cargo lock updater](https://github.com/googleapis/release-please/blob/v17.6.0/src/updaters/rust/cargo-lock.ts)

There is no workspace plugin and no independently configured member release.
This avoids duplicate flat tags and ensures that changes outside `crates/bot`
(including shared domain code and the Worker) are eligible for release notes.

The `0.1.0` manifest is only a seed: it does not assert a published release.
Features and breaking changes advance the minor version while pre-1.0;
fixes advance the patch. Production cutover is the deliberate 1.0.0 boundary.

## Offline verification

`scripts/test-release.cjs` uses a fail-closed mock GitHub client and the
actual repository configuration. It does not use a token or mutate GitHub.
Install its exact library outside the checkout, then run:

```sh
npm install --prefix "$RELEASE_TEST_DEPS" --ignore-scripts --no-audit --no-fund release-please@17.6.0
NODE_PATH="$RELEASE_TEST_DEPS/node_modules" node scripts/test-release.cjs
```

Set `RELEASE_TEST_DEPS` to a disposable dependency directory (in agent runs,
use a directory under `PAPERCLIP_RUN_SCRATCH_DIR`). The fixture covers tagged
and untagged seeds, features in the root/four crates/Worker, breaking changes,
fixes, Common Changelog headings, the card footer, synchronized manifest and
lock updates, and exactly one componentless release candidate. Cargo CI still
validates compilation and the real release flow still validates GitHub writes.

## Required-check dispatch

`GITHUB_TOKEN`-created PRs do not trigger ordinary PR workflows. The release
workflow dispatches `check.yml`, `secret-scan.yml`, and `pr-lint.yml` on the
release PR branch. `GH_REPO` explicitly names the repository because that job
has no checkout. PR lint reads the open release PR's title and body via the
API, then applies the same rules as an ordinary PR event.

No App key or PAT is added to Actions. A reviewer must approve the exact head
SHA and all required checks must be green before squash merge. The first
release is not complete until a release PR has passed those checks, merged,
and the automation has published its tag and GitHub Release.
