# LFG domain and store port

This is the self-contained domain/store portion of TOG-10084. The legacy behavior reference is `TogetherWeOwn/two-bot` at parity revision `d5d11793`, specifically `src/announcements/{service,store,discord}.ts` and migration `0024_announcements_feeds.sql`.

## Delivered seams

- `two_bot_core::lfg`: role parsing, title/start validation, pure signup adjudication, select actions/options, rendering, permission checks, reply text, and stable post nonce.
- `two_bot_core::lfg_store` (`db` feature): guild-fenced posts, ordered roles/signups, capacity-serialized signup/move, idempotent leave/close, and failed-post cleanup.
- `crates/cutover/migrations/0170_lfg.sql`: additive legacy-named `lfg_posts`, `lfg_roles`, and `lfg_signups` tables.

Same-role selection returns `joined` without changing the original signup timestamp. Moving into a full role preserves the existing signup. Close and signup share the same advisory-lock key so a close cannot race a capacity decision. A foreign-guild upsert is refused before role replacement. Closed posts keep existing signups but remove select options and refuse new signups; leave remains allowed as in legacy.

Role keys, duplicate detection, JavaScript numeric slot forms, and UTF-16 title/label limits follow legacy. Keys, labels, slots, and titles use ECMAScript WhiteSpace/LineTerminator trimming, including BOM (U+FEFF) and excluding NEL (U+0085); interior characters are preserved. Message truncation keeps the legacy UTF-16 cap without emitting half a surrogate pair. Start-time input deliberately accepts RFC-3339 ISO-8601 timestamps only; it does not reproduce `Date.parse`'s undocumented natural-language/date-format acceptance.

## Runtime integration still required

The S4 interaction router (TOG-10075) and REST executor (TOG-10076) are not present on the base used by this slice. Do not treat this port as live command coverage. The follow-up must:

1. Register `/lfg`, `/lfg-close`, and `two:lfg:` string selects through the shared router, with configured-guild/default-off announcement gates and builder/runtime `ManageEvents` checks for creation/close. Signup requires no elevated permission.
2. Persist an open post and its roles before posting through the shared executor. Use `lfg_nonce` and reconcile ambiguous acceptance by nonce; remove the guild-scoped post only after unreconciled failure. Save the accepted message ID without replacing roles.
3. Map `LfgSelectAction` to store signup/leave calls. Refresh message content/components after successful changes, suppress mentions (`allowed_mentions.parse = []`), and use ephemeral legacy replies.
4. Record legacy `lfg.create`, signup, leave, and close outcomes in the shared announcement audit store when that S6 seam lands (TOG-9811).
5. Exercise create → signup → full → switch → leave → close via the actual router and mock Discord double, including permission/guild refusals, nonce recovery, and failed-post cleanup. Use only agent-testdb or CI service containers.

No private dispatcher, Discord client, or temporary voice channel feature is included here.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo test -p two-bot-core --features db --locked lfg_store:: -- --ignored --test-threads=1
```

The last command uses only `agent-testdb:5432`, user `agent_test`, database `two_bot_test_tog10084`. It never reads `TWO_DATABASE_URL` or an application credential. In the GitHub job, `TWO_LFG_TESTDB_CI=1` selects the fixed loopback endpoint of the ephemeral PostgreSQL service, which shares the single `agent_test` database with the gateway suite. Tests stay isolated through unique post IDs plus per-test cleanup. The migration runner validates all embedded migration checksums; CI starts with a fresh database and applies the migration before exercising transactions.

The store tests cover round trips, guild fencing, leave, capacity, atomic moves, same-role idempotency, simultaneous joins, closure, and close/signup serialization. The workspace suite uses mock Discord, not a live guild.

## Rollback and release

Revert the feature code and keep announcements disabled until runtime integration is independently reviewed. Migration `0170` is additive: leave the tables in place rather than deleting signup data. No destructive rollback is performed by this slice.

The PR/commits use Conventional Commits and `CHANGELOG.md` has an Unreleased entry. This base has no release-please configuration; no release workflow was added by this feature slice.
