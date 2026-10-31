# Member moderation domain and ledger

This slice implements `/ban`, `/tempban`, `/kick`, `/timeout`, `/warn`, and
scheduled unban processing. The domain and ledger stay independently testable;
live handler registration, Discord REST and the sweep ticker arrive in the
runtime wiring section below, after the shared S4 interaction router and REST
executor merged.

## Integration contract

- Network-facing adapters use `MemberModerationService::execute_with_authority`
  with a resolver for current actor/target roles, permissions and bot hierarchy.
  It acquires the ban/tempban member lane before invoking that resolver, then
  reuses `assert_moderation_allowed` before claiming or preparing an effect.
  Other verbs resolve immediately before validation/claim. The plain `execute`
  seam remains available to trusted callers supplying their own authority facts;
  it cannot make supplied snapshots fresh. Unknown positions must refuse.
  Protected-target refusals share one generic reply. Policy refusals persist the
  actor, target, action and `denied` outcome without claiming or acting; audit
  write failures fail closed. Reasons are trimmed and capped at 512
  UTF-16 code units, matching the legacy JavaScript limit. The final generated
  expiry reason, including its prefix, is also capped at 512 UTF-16 code units
  with Unicode-safe truncation. Tempban bounds
  are 60 seconds–365 days; timeout bounds are 60 seconds–28 days.
- Implement `MemberDiscord` on the shared REST executor, not a second client.
  `DiscordCall` describes the plain-data mutation. Each call must abort after
  **5 seconds**, without automatic retries. Preserve legacy success handling:
  ban 200/204, kick and unban 200/204/404, timeout 200.
- Map definite Discord refusals to `Rejected`; timeouts and transport/5xx
  failures remain uncertain. A definite refusal or a rate limit (Discord did
  not process the request, `DiscordError::proves_no_effect`) permits releasing
  only the failed attempt's destructive claim or requeueing an unban.
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
  not in a bulk reservation before the first Discord await. A per-job Discord
  failure is recorded with its bounded cause and the sweep continues; a
  definite rejection is requeued under the member queue for the next tick,
  never retried within this sweep. Only unknown bot identity and store
  failures stop the sweep. Undispatched later jobs stay pending.
  The policy must carry the resolved `bot_user_id`: without it the sweep returns
  `BotIdentityUnknown` before claiming any job, so no unban runs unattributed.
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
`Rejected`, while timeouts and transport/5xx failures stay
uncertain and keep the claim fenced; a rate limit proves no effect and
releases only the failed attempt. The audit-log reason stays within the
512-unit bound, enforced before any I/O.

For slash commands, the interaction supplies identities and command content, never
dispatch authority. This resolver is wired to the member-command runtime; the signed
internal-action adapter retains its separately owned resolver/activation contract.
After the ban/tempban member-lane wait, the runtime strictly reads the current guild
(id, owner, complete role positions and permission bits), actor, target and bot
memberships. The other verbs perform these same reads immediately before their
validation/claim boundary. Actor permissions are the guild-wide union of current
roles plus `@everyone`, not interaction permission bits or channel overwrites.
Owner and administrator expansion grants every permission, but only the guild owner
is exempt from the actor role hierarchy: an administrator keeps the real top-role
position, as Discord and the website resolver do. Self, protected-target and
bot-hierarchy checks remain. Unknown held roles, missing
role facts, mismatched identities and failed reads refuse with a fixed, attributable
`denied` audit before any prepared intent, warning, expiry or Discord mutation.
Audit-write failures also fail closed. Only a strict target-member 404 permits a
nonmember ban or tempban; kick/timeout/warn require present membership. No 403,
timeout, malformed response or server error is absence. A completed,
hash-matching request whose target has since left (a kick's expected end state)
still replays, nonmutatingly and without a denial: that single read-only lookup
runs only on a confirmed departure, and any other departed-target request refuses.

This closes the avoidable interaction-snapshot and **local member-lane wait** stale
window. The reads are sequential bounded GETs, not an atomic Discord snapshot:
a remote role/membership change after its read (including REST pacing/dispatch
waits) can still race the final effect. Discord's bot hierarchy check is not an
actor-policy recheck. Completed replay remains nonmutating; a failed fresh read
(other than the confirmed departed-target replay above) may refuse a redelivery but
never releases its completed or unknown idempotency row, prepared intent or expiry
fence. No distributed lock or new reconciliation
rule is introduced. **This is not production activation/clearance**: moderation
remains default-off and staging-only, and existing security/soak/release gates
remain mandatory.

All five member verbs share a process-local, per-guild actor cooldown: one new
interaction per actor every 5 seconds, reserved before role REST reads. It applies
to failed attempts too, except malformed invocations: strict option decode runs
before cooldown admission, so a wrong-typed, duplicated, missing, unknown or
invalid option refuses with a `/verb` correction and never consumes the actor
cooldown. The limiter uses a monotonic clock and stores at most 1024 unexpired
actors; capacity exhaustion refuses rather than evicting active cooldowns. Different
actors remain independent; restart clears this local limiter. This does not replace
shared interaction-lane admission or a durable rate limiter. The interaction id is
the idempotency key, so redelivery of that same interaction still reaches durable
replay instead of consuming a new cooldown or moderating twice.

Operator reconciliation ships as `two-bot reconcile-member`
(`crates/bot/src/member_cli.rs`): `--list [--limit <n>]` surfaces prepared
ban intents, running dispatches and quarantined imports via the read-only
bounded `surface_uncertain_limited` store seam (25 rows by default, at most
100; a capped listing ends with a `showing <shown> of <total> rows`
truncation receipt, naming the ceiling past 100 rows since no `--limit`
rerun converges there); `--confirm-ban`, `--reject-ban`,
`--resolve-unban` and `--accept-historical` resolve one exact attempt with
authoritative exact-intent evidence. Mutations are dry-run by default
(`--execute` writes), staging-guild only, and claim tokens never print.
`TWO_MODERATION` stays default-off and staging-only until soak.

## Private saved-expiry inventory

`/tempban-list [page:1..10000]` privately lists saved temporary-ban expiry work,
not observed Discord bans. It shares the configured-guild, moderation and
staging-only member-runtime gates. Both routing and a fresh server-side actor
membership/role check require Ban Members-class authority. No self/member lookup
permission is introduced. This is not live activation or production clearance.

Page one is the default. Pages contain ten complete rows (except the final
page), ordered by exact saved `execute_at`, then work/request ID in PostgreSQL
`C` collation. SQL reads at most eleven rows including lookahead, with a bounded
offset; the next-page invocation is shown when available. Separate invocations
are not a snapshot: concurrent expiry work can shift page boundaries. Page 10000
is the browse ceiling; further inspection belongs to the operator.

Rows show full work and member IDs, a Discord-localized saved timestamp, due or
future timing, and the actual state: `pending`, `staged/prepared`, `running` or
`quarantined`, plus `dispatch uncertain` when saved. Done, cancelled and
superseded history is excluded. A prepared permanent-ban intent without a saved
expiry is not included. **Saved/due does not confirm a Discord unban; running is
not completion.** Neither elapsed time nor this inventory resolves uncertainty.

The reply is ephemeral with mentions suppressed and at most 2000 UTF-16 units.
Unsafe identity characters are percent-encoded as UTF-8 bytes (including `%`),
not clipped; decode those bytes to recover the exact identity. A page whose
complete safe IDs cannot fit fails closed with an operator next action, rather
than dropping rows. Empty inventory, an empty higher page and unavailable reads
have distinct corrective replies. Invalid, duplicated, unknown or wrong-typed
selectors refuse before any expiry read; use `/tempban-list page:1` to restart.

This path performs only SELECTs and authority GETs. It never claims, sweeps,
retries, repairs, deletes, unbans or writes an applied-mutation audit, and never
looks up Discord bans. Reasons, attribution and claim capabilities are not
selected or displayed. Existing reconciliation remains separately authorized.

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

A definite DELETE refusal is recorded and the sweep continues without automatically retrying it until the next tick.
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
post-dispatch failures never do. If that release write itself fails, the caller
still gets the original refusal and nothing is retried: the row stays
`in_flight`, so a retry answers `in_progress` instead of repeating the
mutation. The service logs one `moderation_idempotency_release_failed` error
with only a fixed `action` and `cause` (`discord_rejected`,
`discord_rate_limited`, or `store_rolled_back`), never a key, an id or the raw
store error. Such a line means one caller-supplied key will keep answering
`in_progress`; the signal does
not name it, and no code expires or takes over the row.

### Refused and incomplete request audits

Policy refusals (hierarchy, protected and self targets, permission checks)
audit as `denied` with `stage: validation`; key mismatches use `stage: claim`.
Denials retain only fixed reason copy, not rejected input. Their `:denied`
request identity keeps a corrected retry's plain success row available.
Option-level reason/duration validation never reaches this audit: wrong-typed,
duplicated, missing, unknown and invalid options refuse at strict decode,
before any read, claim or audit write, with a private `/verb` correction and
zero stored rows, so a corrected retry under the same idempotency key lands
exactly one accepted audit.

Discord failures and store errors attempt a best-effort audit with the original
request ID in metadata and the original idempotency key in its dedicated field.
A proven no-effect failure whose key was released writes `:failed` / `failed`;
an uncertain effect, claim or failed release writes `:in_progress` /
`in_progress`. The stage distinguishes `discord`, `store` and `claim`. An
in-flight redelivery uses the same suffix and does not duplicate that row. If
the plain accepted-effect row already exists, it remains the authoritative
observation and no redundant in-progress row is inserted. A completed replay
writes no audit. Audit loss cannot change release/fence decisions or the
original effect error; no failure adds an automatic retry.

Channel moderation's four verbs attempt a best-effort missing-context refusal
row before replying. Known interaction IDs are preserved; unavailable guild or
actor IDs are empty text and an unavailable channel is null, with
`stage: validation` / `cause: missing_context` metadata. No principal is
inferred and no claim or Discord write occurs. Held channel claims write a
separate `:in_progress` row with `stage: claim`, preserving both the original
uncertain dispatch row and its key. Further redelivery deduplicates that row;
completed channel replays still write none. Successful rows are unchanged.

## Recent actions (`/moderation-recent`)

`/moderation-recent [page]` gives staff on shift a private, read-only page of the
newest saved moderation ledger rows. It sits in the moderation command family
behind the same `TWO_MODERATION` gate, configured-guild fence and the Moderate
Members permission as `/warn`, and it needs the member consumer to exist (the
five member verbs share that presence). The router checks the interaction's
permission bits; the handler then rechecks the actor live over REST (guild roles
plus the actor's member) and refuses with a finite corrective when the actor is
missing, cold, unreadable or lacks Moderate Members. Interaction-time bits
never authorize the read, and no member or self authority exists. The reply is
ephemeral with every mention parse disabled.

- **Source.** One guild-scoped, `LIMIT`-bounded read of the newest 100
  `moderation_audit` rows plus `moderation_warnings` rows that no audit row
  shares a `request_id` with, ordered `created_at DESC, request_id ASC`. Claim
  tokens, idempotency keys and metadata are never selected. It is paginated in
  memory; older rows are not listed.
- **Rows.** One line each: complete request id, action, outcome, target (or
  channel), acting staff member, UTC time and a reason of at most 80 UTF-16
  units rendered inside a code span so mentions, markdown and links stay
  inert. The header states these are saved ledger rows, not live-verified
  Discord state.
- **Outcomes are never relabelled.** `banned`, `temporarily_banned`, `kicked`,
  `timed_out`, `warned`, `unbanned`, `purged`, `slowmode_updated`,
  `locked_down` and `unlocked` render `applied`; `denied` and `refused` render
  `denied`; everything else (`failed`, `in_progress`, `unban_failed`, unknown)
  renders `uncertain`. The saved word follows in parentheses.
- **Pages.** Whole rows pack into the 2000 UTF-16 unit budget (a ledger that
  fits stays on one page) with a `Page N of M` footer and, when more pages
  exist, `Next: /moderation-recent page:N+1`. `page` is an optional integer
  1–20; a page outside the packed range (or below 1) answers `Invalid page. Run
  /moderation-recent page:1, then use the next-page hint.` An empty ledger
  answers `No recent moderation actions in this server.`
- **Strict decode first.** Duplicate, unknown and wrong-typed options refuse
  with `Cannot run /moderation-recent: ... Try: /moderation-recent page:1`
  before any Discord or store read, never echoing option values.
- **Failure.** A ledger read failure answers a private `try again` corrective
  with no raw error, and logs the sanitized class only.
- **Pure read.** No claim, sweep, retry, repair, delete, unban, warn, retention
  change, applied-mutation audit, idempotency row or Discord mutation, on every
  path including refusals.

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
