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
`rust`, single `.` package). The root package has a small release-metadata
library at `src/lib.rs` so it is a valid Cargo package, not a targetless
manifest. The native Rust strategy synchronizes the root and all workspace member
versions, local dependency requirements, and `Cargo.lock`. Using one root
strategy includes changes anywhere in the repository and produces one flat
`vX.Y.Z` tag, not one release per crate.

Merge a conventional commit to `main` and release-please opens or updates a
release PR containing the version and root `CHANGELOG.md` updates. Merging
that PR publishes the tag and GitHub Release. Never tag or release by hand.

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

### Database tests

Tests run only on the disposable `agent-testdb` service or a CI Postgres service
container published at `127.0.0.1:5432`, **never staging or production**. Do not
use `DATABASE_URL`, application credentials, credential fallbacks, or libpq
`PG*` connection variables. A configured connection/setup failure must fail the
test, not skip it.

Create one empty bootstrap database `two_bot_test_local` on the disposable
service, owned by its documented `agent_test` principal (empty password, with
`CREATEDB` permission). Then run:

```sh
export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
cargo test -p two-bot-testsupport --locked
cargo test -p two-bot-core --features db --locked --test website_contract --test internal_action_store --test leveling_store
```

The shared guard requires `postgres`/`postgresql`, literal `agent_test:@`, an
allowlisted host, explicit port 5432, and a lowercase `two_bot_test_*` database
name (1–63 bytes, letters/digits/underscores, no trailing underscore). It refuses
credentials, query overrides, fragments, encoded/ambiguous targets, sockets,
and inherited libpq connection settings. SQLx's pgpass fallback is disabled.
The name is a **bootstrap**, not permission to reset that database.

New slices use `crates/testsupport` through a **dev-dependency only**, with the
workspace's release version:

```toml
[dev-dependencies]
two-bot-testsupport = { path = "../testsupport", version = "0.2.0" }
```

```rust,ignore
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn persists_a_row() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await.expect("create migrated fixture");
    // Exercise the store using db.pool(). No hand-written guard or reset DDL.
    // Use db.independent_pool().await for multi-worker/restart assertions;
    // close peer pools before closing the fixture.
    db.close().await.expect("verify teardown");
}
```

Each fixture creates a unique database, applies the supplied migrations, and
closes/drops only that owned database. The bootstrap is never migrated or
dropped. Call `close().await` explicitly so teardown errors fail the test;
`Drop` provides only best-effort cleanup while a Tokio runtime remains alive.
Migration failures clean up immediately. Do not rely on panic/runtime shutdown
cleanup. The lifecycle test proves concurrent isolation and failure cleanup.

Feature-gate store tests with `#![cfg(feature = "db")]`, but do not mark new
DB suites ignored. The normal CI integration step already supplies the single
`two_bot_test_ci` bootstrap and runs integration targets, so new slices need no
new `createdb` name, URL variable, or bespoke workflow step. Website contracts,
internal actions, and leveling are migrated; older suites retain their existing
explicit opt-ins until migrated separately.

The `wrangler/` Worker/DO wrapper has its own `npm ci`, `npm run typecheck`
and `npm test`. Never commit secrets, `.env` files or `target/`. See
[README.md](README.md) for the full service reference.
