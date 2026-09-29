# Contributing

## Commits and PRs

- Squash-merge only. Each PR is one logical change.
- PR title = Conventional Commits header: `type(scope): summary`, at most 100
  characters, no trailing period. Types: `feat`, `fix`, `perf`, `refactor`,
  `test`, `docs`, `build`, `ci`, `chore`, `revert`, `style`, `security`.
- Card ID goes in the body as `Refs: TOG-1234`, never in the title.
- PR body explains what changed, why, and how it was tested (see the PR template).
- `check`, `worker check`, `gitleaks` and `pr-lint` are required checks on `main`.

## Releases

Releases are automated with [release-please](https://github.com/googleapis/release-please)
(`release-please-config.json` + `.release-please-manifest.json`, release-type
`rust`, single `.` package). Merge a conventional commit to `main` and
release-please opens or updates a release PR; merging that PR writes
`CHANGELOG.md`, bumps the root workspace version (member crates are internal
and stay pinned — see `release.yml`), tags `vX.Y.Z` and
publishes a GitHub Release. Never tag or release by hand.

`CHANGELOG.md` uses the [Common Changelog](https://common-changelog.org/)
categories, in its order: **Changed** (`perf`, `revert`), **Added** (`feat`),
**Fixed** (`fix`). `chore`, `docs`, `test`, `ci`, `build`, `refactor` and
`style` stay out of the changelog. Each squash-merged PR title becomes one
entry, so write it for a reader of the changelog: imperative mood, one
user-facing change.

Versioning is SemVer, starting at `0.1.0`; `1.0.0` marks the production
cutover. `feat!` / `BREAKING CHANGE` bumps major (minor while `0.x`).

## Local development

The toolchain comes from `rust-toolchain.toml` (rustup and CI honour it):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

The `wrangler/` Worker/DO wrapper has its own `npm ci`, `npm run typecheck`
and `npm test`. Never commit secrets, `.env` files or `target/`. See
[README.md](README.md) for the full service reference.
