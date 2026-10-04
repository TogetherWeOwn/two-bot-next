# Reconcile a wedged channel moderation lane

An ambiguous Discord write (5xx, timeout, cancellation or unexpected status) can
leave `moderation_channel_executions` and its `moderation_idempotency` claim in
flight. This is deliberate: an automatic retry or expiry could let a delayed
unlock overwrite a newer lockdown. A failed write is **not** proof of no effect.

`two-bot moderation release-channel` is a local operator CLI, not a slash command,
HTTP endpoint, automatic repair job or authority to change a live guild. Use an
already authorized operator execution path and database identity for the intended
environment. The command requires no Discord token, starts no gateway, skips
migrations and never calls Discord. Production incident actions keep their
existing approval gates; integration tests use only disposable test services.

## Reconciliation checklist (before confirmation)

1. Identify the exact guild, channel and stuck interaction/request key. Preserve
   the incident evidence. Confirm the database binding belongs to that environment;
   stop on authentication/permission failure, never substitute credentials.
2. Quiesce every slash/website worker that can act on this channel through the
   existing authorized containment procedure. Ensure the original worker cannot
   resume a planned write. **Wait for all old Discord requests to settle**;
   cancelling a local task does not cancel a request already sent to Discord.
   If settlement cannot be established, leave the durable lane held.
3. Read the channel's **actual Discord state first** using the authorized Discord
   observation surface: guild/channel identity, the complete `@everyone` overwrite
   (existence, exact allow/deny masks), and `rate_limit_per_user` (slowmode). Compare
   these with the intended action and the stored lockdown recovery seed. A purge
   may already have deleted messages; never repeat it just because its result was
   lost. Resolve disagreement and newer/manual lockdowns before unlocking.
4. Run inspection below. Read both matching rows, original action/hash/state,
   timestamps and the generation fingerprint. Ownership `claim_token` values are
   redacted, including in the audit; the fingerprint is not an execution ticket.
   Missing/mismatched rows, `done` claims or inconsistent lane/claim tokens must
   not be repaired with this command. Leave those fenced and investigate.
5. Only after the old write has settled and the channel is reconciled, confirm
   using **the fingerprint from that inspection**, your Discord user ID and a
   bounded explanation of the reconciliation. A changed generation/state refuses
   without committing release; inspect again, do not blindly reuse confirmation.
6. Verify the released audit receipt. The CLI preserves `moderation_lockdowns`
   byte-for-byte and retires the old request to `done/operator_released`, so old
   redelivery replays a no-mutation result. Member erasure (operator or original
   actor) deletes the personal audit rows but never this `operator_released`
   ledger row: it is the replay fence, holding only the key, action, request
   hash, a generic result and timestamps. Resume workers only when safe. A new
   `/unlock` uses the surviving seed and a new request key; do not unlock a newer
   lockdown based on an old incident. Re-observe Discord after any authorized
   follow-up. Releasing a lane does not itself restore masks or reset slowmode.

## Inspect, then explicitly confirm

Use the existing environment-bound `TWO_DATABASE_URL` (or `DATABASE_URL` only
when the primary is absent). Never put credentials in shell arguments or output.
TLS defaults to required; `local-only` is for disposable local test services only.

```sh
two-bot moderation release-channel \
  --guild "$GUILD_ID" --channel "$CHANNEL_ID" --claim-key "$STUCK_INTERACTION_ID"
```

The first line is a JSON report of the lane and ledger, plus
`expected_generation`. Inspection ends with `INSPECTION ONLY: no changes` and
never writes. Read-only migrator credentials suffice for inspection. Copy the
fingerprint into `INSPECTED_GENERATION`, then, through the approved operator path:

```sh
two-bot moderation release-channel \
  --guild "$GUILD_ID" --channel "$CHANNEL_ID" --claim-key "$STUCK_INTERACTION_ID" \
  --confirm-release --expected-generation "$INSPECTED_GENERATION" \
  --operator "$OPERATOR_DISCORD_USER_ID" \
  --reason "Old requests settled; actual overwrites and slowmode reconciled"
```

Confirmation prints the current inspection **before** mutation. It compares that
fingerprint with the earlier read, then compare-and-deletes the channel row by
`claim_token` and retires only the matching `in_flight` ledger generation in one
transaction. An audit insert failure rolls back both. The existing runtime DML
grant covers these three tables; no migrator privilege, new grant or role change
is needed. See the [database role matrix](database-roles.md). The `--operator`
value is attribution, not authentication: the audit also records PostgreSQL's
`session_user` and `current_user` from the authorized database session.

Exit codes: **0** inspected or committed; **1** missing/stale/done/inconsistent
claim or database failure; **2** invalid arguments/configuration. Success emits
`released: true` and `audit_request_id`. On an unknown commit outcome, inspect
before retrying and check `moderation_audit` for the matching channel/key and
`moderation.channel_lane_release` action through the authorized read surface. Do
not interpret a missing lane as permission to repeat a possibly applied action.

## 429 classification follow-up (not changed here)

The slash-runtime history-failure fix treats a mutation HTTP 429 as proven not
applied via `DiscordError::proves_no_effect`. The website executor in
`crates/discord/src/internal_channel_moderation.rs` and member moderation still
classify mutation 429 as ambiguous and retain their claims. History GET 429 is
already safe because no mutation was attempted.

Recommendation: align the mutation classification in a separate reviewed slice,
with single/bulk delete, overwrite, slowmode and member-action regressions proving
429 releases only the applicable attempt while preserving recovery seeds. Keep
5xx, timeout, cancellation and unexpected-status ambiguity fenced. This CLI does
not change any executor's error classification or authorize automatic release.

## Verification

`crates/core/tests/channel_lane_release.rs` covers scoped inspection, redaction,
stale/done/inconsistent claims, audit rollback, recovery preservation and terminal
replay. `crates/cutover/tests/member_erasure.rs` proves that erasing the releasing
operator, then the original actor, deletes their audit rows yet leaves the old key
replaying instead of winning a fresh claim. `crates/bot/tests/channel_lane_release_cli.rs` exercises the real binary:
a mocked overwrite PUT returns 503, inspection writes nothing, confirmed release
preserves the seed, old delivery does not repeat the PUT, and a new `/unlock`
restores the exact original masks. All database connections use the guarded
disposable test service; no real Discord or staging/production database is used.
