# Durable Discord send admission

This is an outbound token-wide gate, not a scheduler, inbound caller throttle,
execution claim, or idempotency store. It deliberately trades availability for
fail-closed behavior. No credentials, receiver listeners, or deployments are
installed by this change.

## Callable interface and authority

`two_bot_core::send_admission` exports `SendAdmission`, `AdmissionPermit`,
`AdmissionError`, `SendCooldown`, `TokenKey` and, with feature `db`,
`PgSendAdmission`.

1. Apply migration `0360_discord_send_admission.sql` through the existing,
   separately authorized migration process. Every consumer of the same token
   must use the **same database authority** as the bot's `TWO_DATABASE_URL`.
   The table is explicitly `public.discord_send_admission`: changing
   `search_path`, guild, channel, caller or process cannot split its lane.
2. Construct `PgSendAdmission::new(pool.clone(), bot_token)` and share it through
   `Arc<dyn SendAdmission>`. Identity is SHA-256 of the domain separator
   `two-bot-next/discord-bot-token/v1\0` plus the canonical credential (one
   optional `Bot ` prefix removed). Callers cannot choose a namespace. The
   credential is high-entropy; its fingerprint is not a reversible secret.
   Neither raw credentials nor provider text/content are persisted. Debug
   output redacts the identity and authenticated transports' credentials.
3. `admit().await` atomically commits an occupied generation before any HTTP.
   Busy, finite-held, or indefinite-held lanes return `AdmissionError::Blocked`;
   unavailable/missing storage returns `Storage`. This interface does not wait
   or poll for another sender. Callers report/defer new independent work.
4. `permit.complete(cooldown).await` consumes the permit and atomically extends
   the hold **and** releases occupancy, conditional on the exact generation.
   External `PgSendAdmission::extend` is monotonic and cannot release a claim.
   Completion never clears an existing indefinite or longer finite hold.

Finite deadlines use the database clock. All Discord channel/bucket cooldowns
are conservatively promoted to the whole token. Both header and body timing are
considered; the longer valid delay wins. Missing/unrepresentable timing installs
an indefinite hold, not the legacy guessed/capped sleep. A legacy local sleep
may still be shorter: the next attempt must acquire the gate again and is refused
until the durable hold allows it.

## Transport/bootstrap handoff

- **Announcement receiver:** use
  `AnnouncementExecutor::with_admission(twilight, channel_keys, admission)`.
  It rejects a gate for another credential. The old `new` cannot send to live
  Discord without admission; it remains usable for private loopback fixtures.
  The receiver owns authorization, durable effect claims, receipt/audit
  persistence and HTTP responses. It must persist `RateLimited` as **terminal
  definitive no-effect**, even if admission completion reports a storage fault,
  and never resend that idempotency key. A later independent intent can be
  refused as `NoEffect(Refusal::SendAdmissionBlocked)` before wire I/O.
  `Unknown` retains both the receiver's effect claim and the gate's occupancy.
  The transport still has one attempt, no redirects/status/connection retries,
  one ten-second deadline, and a 64-KiB response cap.
- **ActionExecutor/sticky:** use
  `ActionExecutor::with_admission(token, optional_proxy, admission)`.
  `StickyRuntime::from_env` constructs this gate using its existing runtime
  pool. Every actual request, including callbacks, command publication,
  sticky posts/deletes and every retry, uses the raw governed transport. Its
  bounded response cap is 8 MiB for list/history responses. Local legacy pacing
  is additional, never the authority. Ungoverned constructor paths accept only
  explicit loopback HTTP fixtures; real Discord is refused.
- **Cutover:** the three Discord-reading CLI bootstraps call
  `RestClient::from_env`. It requires the runtime `TWO_DATABASE_URL` authority
  even if a data import target is different. Twilight builds requests/models
  but its `ResponseFuture` is not awaited: hidden Twilight 429 resends would
  bypass the gate. Each raw attempt is admitted independently, with a 30-second
  bound. Unknown transport failures stop rather than granting a new lease.
- **Guild-config snapshot/restore:** bootstrap constructs
  `GuildConfigDiscordApi::with_admission` from `TWO_DATABASE_URL` before the
  first identity/preflight request, including dry runs. Every authenticated
  request and retry participates. Capture's four bot-token reads are sequential
  so they do not compete with each other. CDN emoji reads and S3 requests do not
  use a bot credential and are not put in this lane.

Loopback fixture constructors are not an operational bypass: all shipped runtime
and CLI bootstraps inject admission even when given a loopback proxy. Do not
activate an old ungoverned binary, legacy bot, or external same-token client
alongside this runtime. Cutover to these guarded versions retains the existing
owner-approved activation/deployment boundary; this code cannot police arbitrary
old/external programs or databases deliberately configured as a second authority.

## Cancellation, crash, unavailable storage and recovery

Permits have **no TTL and no drop-release**. The database does not hold a
transaction/connection across HTTP. Dropping/cancelling a send, losing the
process, an uncertain exchange, or losing completion storage leaves durable
occupancy. A failed completion cannot silently open another lane: its single
statement either installs the hold and clears occupancy together, or the old
occupied row remains. A lost completion acknowledgement does not justify
replaying the effect. Unknown or old generations are not execution leases.

Indefinite holds and abandoned claims require explicitly authorized
reconciliation. There is intentionally no automatic expiry, startup reset, or
"force send" switch. Before any manual release, fence **all** credential users,
prove the old send cannot continue, reconcile the effect/provider cooldown,
and record evidence for the exact generation. Do not delete/reset the lane,
rotate a credential, or release an indefinite hold merely because a process is
old or a receiver idempotency claim is stale. Those operations are not authorized
by this implementation. Persist this table across restarts and authority
failover; a blank replacement database is not recovery evidence.

The existing receiver HMAC provisioning, staging/production deployment and
activation holds remain unchanged.

## Verification (no live Discord or production/staging DB)

Service tests require the guarded disposable `TestDatabase` fixture with an
explicit `TWO_TEST_DATABASE_URL` naming the passwordless `agent_test` principal
on `agent-testdb:5432` and a `two_bot_test_*` bootstrap. Each test owns a newly
created database; cross-pool tests open independent pools into that same database.
Only generated fake tokens and loopback mock HTTP are used.

```sh
# Controller compiling commands MUST use the approved cache wrapper.
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test send_admission -- --ignored
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db-tests --lib admission_ -- --ignored
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test send_admission -- --ignored
```

The existing hosted `check` service lane runs these explicitly. The core cases
cover atomic cross-pool admission, restart/claim persistence, monotonic finite
extension, indefinite/overflow holds, storage failures, cancellation, stale
completion and finite expiry. Mock transport cases cover successful release,
announcement → ActionExecutor/backup blocking, ActionExecutor → announcement
indefinite blocking, per-retry checks, cancellation/restart safety, no pre-send
I/O on storage/configuration failure, and definitive single-attempt 429 despite
completion storage failure. Existing announcement tests retain the single-attempt,
ten-second default, response-bound, scope and unknown-outcome contracts.
