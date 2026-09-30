# Member moderation domain and ledger

This slice implements `/ban`, `/tempban`, `/kick`, `/timeout`, `/warn`, and
scheduled unban processing. It does **not** register live handlers, instantiate
a Discord HTTP client, or start a scheduler. It stays independently testable
until the shared S4 interaction router and REST executor merge.

## Integration contract

- Feed current actor/target roles and permissions into `MemberExecution`.
  `MemberModerationService::execute` reuses `assert_moderation_allowed`; it
  validates before claiming or acting. Reasons are trimmed and capped at 512
  UTF-16 code units, matching the legacy JavaScript limit. The final generated
  expiry reason, including its prefix, is also capped at 512 UTF-16 code units
  with Unicode-safe truncation. Tempban bounds
  are 60 seconds–365 days; timeout bounds are 60 seconds–28 days.
- Implement `MemberDiscord` on the shared REST executor, not a second client.
  `DiscordCall` describes the plain-data mutation. Each call must abort after
  **5 seconds**, without automatic retries. Preserve legacy success handling:
  ban 200/204, kick and unban 200/204/404, timeout 200.
- Map definite Discord refusals to `Rejected`; timeouts, transport/5xx failures
  and rate limits remain uncertain. Only a definite refusal permits releasing
  a destructive claim or retrying an unban.
- Keep `TWO_MODERATION` off by default and restrict enabling it to staging.
  No production guild or token was used to verify this slice. The service is
  deliberately gate-independent: the shared router owns runtime gating.
- Construct `PgMemberModerationStore::new(pool, guild_id)` **once per guild
  consumer**, cloning it for command and sweep paths. The store rejects foreign
  guild writes, recovery and claims. Clones share local per-member FIFO queues;
  they are not cross-process Discord-effect locks. Do not run independent
  consumers for the same guild. The memory double can share one set of queues
  across several guilds but still scopes every sweep to its supplied guild.
- Drive `run_due_unbans(guild_id)` every `UNBAN_SWEEP_INTERVAL_SECONDS` (30),
  with at most 25 jobs per sweep. Each is claimed immediately before processing,
  not in a bulk reservation before the first Discord await. A failure stops the
  sweep; a definite rejection is requeued under the member queue for the next
  tick, never retried within this sweep. Undispatched later jobs stay pending.
- Surface prepared/uncertain ban intents and `running` jobs for reconciliation.
  Never silently take them over. New permanent and temporary bans refuse while
  any `running` schedule exists for that guild/member: a timed-out or cancelled
  DELETE can still land after a new PUT. Under the member queue, only
  `resolve_uncertain_unban(request_id, claim_token, resolution)` may close this
  uncertainty. `Completed` requires proof the DELETE finished; `Void` requires
  proof it cannot still land, and requeues a still-required accepted expiry with
  a fresh dispatch token on its next sweep. Only an expiry replaced by a newer
  accepted ban is superseded; voiding a DELETE does not cancel the original
  temporary ban's expiry obligation. Current banned status, elapsed time or cancellation
  of a local task is not that proof. Disabling moderation must not abandon
  outstanding staged/pending/running or quarantined expiries.
- S5 owns authenticated audit-reason markers and the operational audit mirror.
  Until that integration, the Discord reason is the plain moderator reason.

## Durability

Migrations 0110–0112 stay in `crates/cutover/migrations`, the directory embedded
by the S6 migration runner. Existing warnings, scheduled unbans, audits and
idempotency retain legacy table/column names. Migration 0111 adds
`moderation_member_bans`, a separate ban-intent ownership ledger. Migration 0112
converts all eight legacy TEXT timestamp columns to timestamptz, preserving
history, tokens and quarantine states without inferring acceptance. It is
repeat-safe; an invalid timestamp aborts rather than silently losing evidence.
`PgMemberModerationStore` is available
under core's optional `db` feature; framework-free tests use `MemMemberStore`
and `MockMemberDiscord`.

Claims are atomic on `(guild_id, idempotency_key)` and bind the key to the
validated request content. A completed request replays its outcome. An
uncertain request remains `in_flight`, without timer takeover. SQL errors expose
only a classification/SQLSTATE, never bound parameters or database DETAIL.

Both permanent and temporary bans hold the same member queue as the unban
sweep and persist a **prepared** intent before Discord. Tempbans atomically
stage their expiry with that intent. Its monotonically allocated generation
fences all older expiries immediately, including during a live or uncertain
permanent ban. Generations are durable execution order, not timestamps or
lexicographical request IDs; clock ties or clock rollback cannot pick a winner.

Observed Discord acceptance is explicitly recorded with `confirm_ban` before
expiry activation. Confirmation atomically supersedes strictly older staged or
pending schedules, never dispatched `running` rows or authoritative terminal
states. A permanent ban therefore cannot be undone by an earlier undispatched
tempban expiry. A safe
refusal atomically records **rejected** and cancels this request's staged expiry;
older accepted expiries become eligible again. If that transaction fails, the
prepared fence remains. It is NOT promoted as a successful ban by a later sweep.

Recovery activates only staged rows with durable **accepted** intent and the
latest non-rejected generation, under the owning guild's member queue. If a
process exits before recording acceptance, the result is unknowable; it needs
reconciliation rather than an automatic unban that might undo an existing
permanent ban. This is a deliberate correction to legacy blind staged recovery.
Once acceptance is recorded, activation failures/crashes recover safely.

Overlapping sweeps atomically claim due accepted/current jobs with
`FOR UPDATE SKIP LOCKED`. Claim tokens and current generation fence stale
ownership/completion/requeue attempts. Only the job immediately being processed
becomes running; process loss may make that one job uncertain, but does not
strand the remaining batch. No running claim is reclaimed by age.

Migration 0111 quarantines active imported schedules without a matching trusted
intent; it neither invents acceptance/order nor deletes their history. Do not
backfill generations by wall time or assume a currently banned user proves
which request Discord accepted. Reconciliation requires authoritative evidence
for the exact intent (including guild/member/request and generation), then
recorded acceptance or definite refusal under the same member queue. Where that
evidence is unavailable, retain the fence and escalate for a recorded security
disposition. The runtime reconciliation/operator workflow is not implemented by
this domain/store PR and must exist before enabling moderation.

Accepted ban PUTs are audited immediately after observed Discord acceptance,
before ownership confirmation or expiry activation. Accepted scheduled DELETEs
are audited under the member queue while the dispatch token is still held,
before both normal and failed completion writes. Audit loss is logged and
cannot cause a duplicate mutation. A completion-write failure keeps the
destructive claim uncertain. A definite pre-dispatch transaction rollback or
no-write result releases the idempotency key for retry; ambiguous commit or
post-dispatch failures never do.

## Verification

```sh
cargo test -p two-bot-core --locked
cargo clippy -p two-bot-core --all-targets --features db --locked -- -D warnings
MEMBER_TESTDB=agent-testdb cargo test -p two-bot-core --features db --locked \
  --test member_moderation_db -- --ignored
```

The database tests accept only `agent-testdb:5432` as `agent_test` with an empty
password, or the ephemeral CI Postgres service (`MEMBER_TESTDB=ci` inside GitHub
Actions). They never read `DATABASE_URL`, Discord credentials, or inherited
credential fallbacks. Each run uses and cleans its own generated scratch schema.
The CI `moderation db` job executes these explicitly ignored integration tests.
