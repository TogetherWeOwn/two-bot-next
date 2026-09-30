# Leveling domain and PostgreSQL runtime

Leveling uses plain-data decisions in `two-bot-core::leveling`, async persistence
in `two-bot-core::leveling_store` (feature `db`), and the ordered S3/S4 bridge in
`two-bot-discord::leveling_runtime`. Configured gateway startup now installs that
bridge with the shared interaction router and REST executor. Command publication
still belongs to the shared registry; this integration does not bulk-overwrite
guild commands or add a second registry.

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

## Ordered runtime integration

`DeferredLeveling` collects requests only when the synchronous S3 handlers call
their existing hook. `OrderedLevelingPipeline` holds a Tokio dispatch mutex over
session/cache transitions, draining requests, awaited sqlx awards and REST
reward effects. There is no `block_on`, detached award task or synchronous lock
held across an await. This single-shard serialization preserves member ordering
and READY/RESUMED/member-removal barriers. Unknown or reconnect-spanning voice
duration is never reconstructed or awarded. `MessageEligibility` carries the
existing staff-automation/capture-only guards for callers that classify messages.

The configured bot dispatches through this wrapper before committing the existing
funnel/checkpoint transaction, under the gateway's heartbeat-safe deadline. Store
or executor errors propagate to the supervisor and make readiness unavailable;
logs do not expose SQL connection details, tokens or message bodies. XP
projection/cooldown/audit remain one store transaction, **not** a transaction with
Discord or the gateway checkpoint. A REST failure can leave committed XP without
its role grant; no durable reward outbox/retry is claimed by this slice. A future
level-up re-reads the whole earned ladder and can reconcile missing grants.

The shared router owns `/rank [member]` and `/leaderboard`; the runtime maps
`profile`/`leaderboard` through the existing reply functions and calls the shared
executor's interaction callback. Rank uses an ephemeral response. Leaderboard
always uses limit 10, suppresses mention parsing and has no paging components.
Other routes are left for their owning feature. Startup strictly validates
`TWO_ONBOARDING_MODE` with the existing gates; session mode and onboarding dry-run
both suppress reward writes without suppressing XP.

On an actual level-up, the runtime reads the current ladder and member roles,
plans grants with `staging_revoke_allowed = false`, and sends idempotent role PUTs
through the shared executor. It sends no channel announcement. Ordinary runtime
never constructs a revoke fence. Separately authorized callers must supply
`StagingRevokeFence` with distinct staging/production identities; production and
nonstaging guilds are rejected before any I/O. For a nonempty revoke set, current
bot identity/roles and the guild role catalog must prove Manage Roles (or
Administrator), a complete catalog, nonmanaged targets and strict hierarchy for
**every** target before any grant/removal is sent. Discord permission changes
after that preflight can still fail individual mutations; errors are propagated,
not treated as successful or atomic remote effects.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features two-bot-core/db --locked -- -D warnings
cargo test --workspace --features two-bot-core/db --locked
cargo test -p two-bot-core --features db --locked --test leveling_store -- --ignored
cargo test -p two-bot-discord --features db --locked --test leveling_roles
cargo test -p two-bot-discord --features db --locked --test leveling_runtime -- --ignored --test-threads=1
```

The ignored leveling targets connect only to `agent-testdb:5432`, user/database
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

Runtime proofs use synthetic Twilight gateway events, the existing local mock
REST double and an isolated approved test schema: independent concurrent sources
produce exactly two audit rows and one threshold crossing; duplicate dispatches
preserve totals; delayed role reads yield the current-thread Tokio executor while
later member events remain ordered. Tests assert reply text, optional-member
fallback, fixed top 10, mention suppression, no announcements, eligibility and
unknown-duration guards, session suppression, observable executor failures,
idempotent grant readback and whole-set revoke refusal. No staging deployment,
command publication or live Discord verification is claimed.
