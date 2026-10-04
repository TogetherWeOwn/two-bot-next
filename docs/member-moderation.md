# Member moderation domain and ledger

This slice implements `/ban`, `/tempban`, `/kick`, `/timeout`, `/warn`, and
scheduled unban processing. The domain and ledger stay independently testable;
live handler registration, Discord REST and the sweep ticker arrive in the
runtime wiring section below, after the shared S4 interaction router and REST
executor merged.

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
  any prepared PUT or `running` DELETE exists for that guild/member. An unfinished
  old PUT may land after a newer temporary ban expires, just as an unfinished
  DELETE may land after a new PUT. Every prepared PUT also fences expiry selection,
  recovery and dispatch, regardless of whether its generation is older or newer.
  Staging returns a `BanAttempt` before dispatch. Keep its generation with the
  PUT's evidence, including its guild/member/request identity. Under the member
  queue, `confirm_ban_attempt` / `reject_ban_attempt` may reconcile that exact
  prepared attempt only with proof the PUT finished or provably cannot still land;
  neither a current banned snapshot nor local cancellation supplies that proof. Under the member queue, only
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

## Runtime wiring

The runtime slice registers `/ban`, `/tempban`, `/kick`, `/timeout` and
`/warn` through the shared router into one per-guild `MemberRuntime`
(`crates/bot/src/member_runtime.rs`), built once at boot via
`MemberRuntime::from_env` and cloned across command dispatch and the
supervised `member_unban_sweep` job (30 s cadence, 25 jobs per tick, wired in
`crates/bot/src/website_jobs.rs`). No second same-guild consumer exists:
commands and sweep share one store `Arc`, so bans and unbans for one member
serialize through one set of local queues.

`MemberDiscord` is implemented on the shared REST executor
(`crates/discord/src/member_moderation.rs`): one attempt per verb, the legacy
5 s abort, no automatic retry. Kick and unban treat 404 as complete; definite
refusals (including local guard refusals, which never reach the wire) map to
`Rejected`, while timeouts, transport/5xx failures and rate limits stay
uncertain and keep the claim fenced. The audit-log reason stays within the
512-unit bound, enforced before any I/O.

Interaction-to-`MemberExecution` mapping resolves live role facts (guild
roles/owner, resolved-or-fetched target member, cached bot id); incomplete
snapshots refuse fail-closed instead of guessing hierarchy. The interaction
id is the idempotency key, so Discord redelivery replays the stored outcome
instead of moderating twice.

Operator reconciliation ships as `two-bot reconcile-member`
(`crates/bot/src/member_cli.rs`): `--list` surfaces prepared ban intents,
running dispatches and quarantined imports via the read-only
`surface_uncertain` store seam; `--confirm-ban`, `--reject-ban`,
`--resolve-unban` and `--accept-historical` resolve one exact attempt with
authoritative exact-intent evidence. Mutations are dry-run by default
(`--execute` writes), staging-guild only, and claim tokens never print.
`TWO_MODERATION` stays default-off and staging-only until soak.

## Durability

Migrations 0110–0114 stay in `crates/cutover/migrations`, the directory embedded
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

A definitely rejected request may reuse its request ID, but staging returns a
fresh attempt generation. Every confirmation and rejection compares the expected
attempt in the mutation itself. Delayed evidence for the previous attempt cannot
accept the retry, reject it, or cancel/supersede any of its expiry state. The old
identity-only `confirm_ban` / `reject_ban` methods remain callable but **always fail
closed**; migrate callers to the attempt-fenced methods. Never attach old evidence
to a generation fetched from the current row. A member queue alone does not
identify which retry an outcome describes.

An older prepared PUT with authoritative evidence of success **before** a later
accepted PUT has a separate terminal path: `resolve_historical_ban_acceptance`.
The trusted operator workflow must authenticate acceptance and remote completion
ordering for both exact attempts before constructing `HistoricalBanAcceptance`.
It supplies the later request/generation, actor, and bounded non-secret durable
acceptance/ordering evidence IDs; the store validates identities, not external
proof authenticity. Local generations only prevent ownership takeover; they do
not prove remote order. Missing/unknown ordering stays fenced and requires a
recorded security disposition, never a false rejection of an accepted effect.

Under the same member queue, historical resolution atomically records a separate
`accepted_historical` audit and marks only the exact older intent accepted.
The later accepted intent remains the owner; its schedule is untouched. Only the
older PUT's never-dispatched expiry is superseded. Running or imported-uncertain
DELETE evidence is preserved and still fences mutations. Audit failure or
collision rolls back the entire resolution. This API neither completes the
original idempotency key nor enables a runtime/operator command.

Observed Discord acceptance is explicitly recorded with `confirm_ban_attempt` before
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

Overlapping sweeps read advisory candidates, then claim each due accepted/current
job with a conditional UPDATE under its member queue. Staging and the transition
to dispatch ownership therefore cannot interleave. Ownership/eligibility is
rechecked after the queue wait; a replaced candidate never becomes running.
Cancelling a queue waiter leaves it pending without a dispatch token. Claim tokens
and current generation fence stale ownership/completion/requeue attempts. Only
the job immediately being processed becomes running; process loss after this
transition may make that one job uncertain, but does not strand the remaining
batch. No running claim is reclaimed by age. `claim_due_unbans` acquires member
queues internally; do not wrap it in another `serialize_member` callback.

A definite DELETE refusal still ends that tick without automatically retrying it.
Migration 0113 adds a nullable `retry_generation` queue ticket. Exact-token requeue
allocates the next value from the existing ban-generation sequence, then clears
only the safe dispatch claim. Due jobs sort by that ticket (or their original
intent generation), so a persistently refused member yields to existing work,
while later arrivals cannot continually push its retry back. The original expiry
and PUT ownership generation are unchanged; sequence gaps do not prove remote
order. Memory mirrors SQL even with a frozen/backwards clock. Retry tickets are
preserved in backups; restore resets the shared sequence above both high-water
marks. Unknown/timeout/cancelled dispatches remain running/fenced and never gain
retry tickets from elapsed time. Migration 0114 supplies the narrow member-ledger
grants for an existing runtime group; the explicit role matrix and privilege tests
also cover the member relations and attached generation sequence.

Migration 0111 quarantines active imported schedules without a matching trusted
intent; it neither invents acceptance/order nor deletes their history. Its separate
`dispatch_uncertain` marker preserves every imported running DELETE fence even
when no claim token survived. Previously quarantined rows with a token/claim time
are also conservatively fenced. Quarantine never makes such a member safe for a
new PUT or another DELETE. Exact-token reconciliation can clear an imported
DELETE's uncertainty: Completed closes it, while Void leaves the unknown expiry
quarantined without inventing acceptance or cancelling its obligation. A missing
token cannot be resolved through this API and requires a recorded security
disposition; elapsed time never clears it. Replaying the migration preserves
resolved markers and keeps all imports non-executable. Do not
backfill generations by wall time or assume a currently banned user proves
which request Discord accepted. Reconciliation requires authoritative evidence
for the exact intent (including guild/member/request and generation), then
recorded acceptance or definite refusal under the same member queue. Where that
evidence is unavailable, retain the fence and escalate for a recorded security
disposition. The runtime reconciliation/operator workflow ships in the runtime
wiring slice (`two-bot reconcile-member`, see below) and must exist before
enabling moderation.

Accepted ban PUTs are audited immediately after observed Discord acceptance,
before ownership confirmation or expiry activation. Accepted scheduled DELETEs
are audited under the member queue while the dispatch token is still held,
before both normal and failed completion writes. Audit loss is logged and
cannot cause a duplicate mutation. A completion-write failure keeps the
destructive claim uncertain. A definite pre-dispatch transaction rollback or
no-write result releases the idempotency key for retry; ambiguous commit or
post-dispatch failures never do.

## Verification

On the persistent controller, use the bounded cache wrapper from this isolated
workspace; a refused lease is not permission to compile directly. Hosted CI keeps
its existing Cargo commands. See [the build-cache runbook](build-cache.md).

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets --features db -- -D warnings
MEMBER_TESTDB=agent-testdb python3 scripts/cargo_cache.py run -- test \
  -p two-bot-core --features db --test member_moderation_db -- --ignored
```

The database tests accept only `agent-testdb:5432` as `agent_test` with an empty
password, or the ephemeral CI Postgres service (`MEMBER_TESTDB=ci` inside GitHub
Actions). They never read `DATABASE_URL`, Discord credentials, or inherited
credential fallbacks. Each run uses and cleans its own generated scratch schema.
The CI `moderation db` job executes these explicitly ignored integration tests.
