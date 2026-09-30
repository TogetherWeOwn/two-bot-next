# Leveling domain and PostgreSQL runtime

This S4 slice implements plain-data leveling decisions in `two-bot-core::leveling`
and async persistence in `two-bot-core::leveling_store` (feature `db`). It does not
publish commands, dispatch interactions or perform Discord REST writes.

## Legacy contract

The frozen parity source is [two-bot d5d11793](https://github.com/TogetherWeOwn/two-bot/tree/d5d11793):
[`service.ts`](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/src/leveling/service.ts)
and [`discord.ts`](https://github.com/TogetherWeOwn/two-bot/blob/d5d11793/src/leveling/discord.ts).

- Message awards: 15 XP, one per member per 60 seconds. Voice awards: 5 XP per
  completed minute, with an independent 60-second source cooldown.
- MEE6 curve and imported XP use the existing ladder. XP never exceeds
  `9_007_199_254_740_991`; a rejected award does not consume its cooldown.
- `/rank [member]`: ephemeral; display name is `globalName ?? username`. XP,
  progress, span and remaining XP are grouped with commas; level, rank and
  member count are not grouped. Missing members read zero XP and rank below
  members holding XP. XP ties use stored member IDs ascending.
- `/leaderboard`: public top 10, mention parsing suppressed; empty text is
  `No XP has been earned yet.`. The store supports a limit clamped to 1–25.
  Legacy has no offset/cursor paging or page components; none are invented here.
- Legacy level-up processing grants earned roles but posts **no channel
  announcement**. The reply/effect types intentionally do not add one.
- Reward grants skip already-held roles. Session onboarding suppresses all
  reward-role writes. Re-planning after applying a plan yields no new grants.
- Revocation is separate from an ordinary level-up. The later legacy
  [`removeLevelRoles`](https://github.com/TogetherWeOwn/two-bot/blob/a74160b8deee128bc9a6998a1e730fbeabef4bc6/src/leveling/discord.ts#L68-L124)
  is staging-only and fail-closed on Manage Roles and role hierarchy. The
  planner requires an explicit staging-revoke gate; the executor must validate
  the entire revoke set before applying any removal. Audit reasons match that
  legacy operation. Non-ladder roles are never targeted.

## Store invariants

Reuse `crates/cutover/migrations/0002_leveling.sql`; no new migration is required,
so this card's reserved 0100–0109 range remains unused. Import runs and XP
replacement remain in `two-bot-cutover`; organic awards never overwrite
`imported_xp`.

One transaction claims the source cooldown, upserts the XP projection with the
ceiling guard, then writes the audit row. Any failure rolls everything back.
The member upsert's returned total determines the previous level (`total -
amount`), so simultaneous message/voice awards cannot both report the same
threshold crossing. Losing a cooldown race reads the winner's committed total.
Reward configuration replacement is transactional, last row per level wins,
and the legacy unique-role constraint remains enforced.

`xp_awards.xp` and reward levels retain the existing INT4 columns. An award too
large for the audit column returns a database error with no projection/cooldown
write retained; reward levels outside INT4 range fail input validation.

## Integration handoff

At base `db47381`, the router and REST executor slices are not merged.
`handlers::LevelingHook` and `discord::Pipeline::handle` are synchronous, while
these sqlx functions are async. Do not block a Tokio runtime or substitute a
private dispatcher/client to bridge them.

The follow-up integration must:

1. Use the S4 interaction router for `/rank` and `/leaderboard`, mapping
   `profile`/`leaderboard` through `rank_reply`/`leaderboard_reply`.
2. Await `award_message` and `award_voice` from the S3 eligibility/session path
   through an ordered async gateway bridge. Preserve bot/webhook/staff/capture
   filtering, measured-duration requirements and per-member event ordering.
3. On `XpAward.leveled_up`, resolve the current member roles and reward ladder,
   apply the onboarding gate, and pass `RewardRolePlan` to the S4 executor.
   Ordinary level-ups use `staging_revoke_allowed = false`.
4. Enforce the explicit staging fence and all-or-nothing permission/hierarchy
   checks for a separately authorized revoke operation. Test side effects only
   against the mock Discord double, never the production guild/token.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features two-bot-core/db --locked -- -D warnings
cargo test --workspace --features two-bot-core/db --locked
cargo test -p two-bot-core --features db --locked --test leveling_store -- --ignored
```

The last command connects only to `agent-testdb:5432`, user/database
`agent_test`, empty password, and creates a random isolated schema. It never
reads `DATABASE_URL` or inherited application credentials and never falls back
on connection failure. CI runs the same tests against its credential-free
Postgres service container using all three explicit CI flags. No tests contact
Discord, staging databases or production databases.

The checked-in golden fixture executes the frozen legacy functions, covering
104 level thresholds, 210 XP samples (threshold-minus-one and the storage
ceiling), and exact rank reply strings including large ungrouped ranks/counts.
Regenerate with Node 24 in an en-US locale:

```sh
# Use a run-owned scratch directory; these public files contain no credentials.
mkdir -p "$PAPERCLIP_SCRATCH_DIR/leveling-legacy"
curl -fsS https://raw.githubusercontent.com/TogetherWeOwn/two-bot/d5d11793/src/leveling/service.ts \
  -o "$PAPERCLIP_SCRATCH_DIR/leveling-legacy/service.ts"
curl -fsS https://raw.githubusercontent.com/TogetherWeOwn/two-bot/d5d11793/src/leveling/discord.ts \
  -o "$PAPERCLIP_SCRATCH_DIR/leveling-legacy/discord.ts"
node crates/core/tests/fixtures/generate_leveling_legacy.mjs \
  "$PAPERCLIP_SCRATCH_DIR/leveling-legacy" > crates/core/tests/fixtures/leveling_legacy.json
```

The Postgres proofs cover 59/60-second boundaries, voice minutes, profile and
leaderboard ordering/text, ceiling rollback, reward replacement rollback,
threshold grants, concurrent first-award races, independent-source races,
imported-XP preservation, zero/oversized no-ops and audit-insert rollback.
