# Moderation/automation disable guard

Disabling moderation (`TWO_MODERATION` unset) or automations (`TWO_AUTOMATIONS`
unset) while Postgres still owes releases strands members in bans and
channels in lockdown silently. The disable guard refuses that boot.

## What it checks

When either gate is off at boot, the container reads the owed state from
Postgres — never Discord — before starting the gateway:

- `moderation_scheduled_unbans` rows outside the terminal states
  (`done`, `superseded`, `cancelled`) are owed tempban releases; rows left
  `running` get their own tagged section (see
  [Stranded `running` unban claims](#stranded-running-unban-claims));
- every `moderation_lockdowns` row is an active lockdown holding recovery
  state;
- every enabled `scheduled_messages` row would stop firing (read only when
  the table exists).

Any owed release refuses boot (exit 1, `moderation_disable_refused`), naming
the outstanding request, channel, and schedule ids. Each section states its
exact count and names at most 10 ids; `+N more` appears only when ids were
actually cut, so a section of exactly 10 names all 10 and 11 ends in `+1 more`. While both gates are on,
the guard short-circuits before any database read. Missing tables read as
empty; any other read failure refuses closed (`moderation_disable_unknown`).

## Operator check

```sh
two-bot moderation preflight [--json] [--allow-owed]
```

Exit 0 is CLEAR, 1 is REFUSED, 2 means the state could not be read. The
check is read-only: it opens `TWO_DATABASE_URL` (`DATABASE_URL` when unset)
without migrating and never contacts Discord. `TWO_DATABASE_TLS` applies as
usual.

`--json` keeps the existing `pending_unbans`, `active_lockdowns` and
`enabled_scheduled` arrays, and adds `running_unbans`: the ids in
`pending_unbans` whose state is `running`. In text, those ids leave the
`pending unban(s)` section and appear in their own
`running unban claim(s) [running]` section, so a stranded claim is never
hidden behind a truncated pending list. A refusal with no `running` row has
neither the tag nor the recovery pointer. Exit codes are unchanged.

## Stranded `running` unban claims

A tempban release is `running` while one worker holds its claim token and has
sent (or may have sent) the Discord DELETE. If that worker stops before it
records the outcome, the row stays `running`. Nothing reclaims a `running`
claim by age or on restart, because the DELETE may still land, so it never
drains on its own. It also keeps refusing a new ban for that member, and the
disable guard keeps refusing boot. A `pending` row, by contrast, is picked up
by the next sweep once moderation runs again. A `quarantined` import with a
`dispatch_uncertain` marker closes through the same steps, though the report
lists it with the pending ids.

Recovery, in order:

1. **Confirm it is stranded.** Stop the worker, then list the claims
   (read-only; do not paste member ids into tickets or chat):

   ```sql
   SELECT request_id, guild_id, user_id, claimed_at
     FROM moderation_scheduled_unbans WHERE state = 'running' ORDER BY request_id;
   ```

   A live worker normally settles a `running` row within a sweep (30 s), so
   re-run after the container is down.
2. **Check the Discord ban list** for that member: Server Settings, Bans,
   and the audit log's member-unban entry for them. This is the evidence to
   collect. A current banned or not-banned status alone does not prove the
   DELETE finished or cannot still land, and neither does elapsed time or
   killing the worker.
3. **Choose the outcome the evidence proves.**
   - The unban provably finished: `Completed`. The row becomes `done`.
   - The DELETE provably cannot still land: `Void`. The row returns to
     `pending` for the next sweep while its ban is still the member's current
     accepted ban, and becomes `superseded` once a newer accepted ban replaced
     it.
   - Neither is provable: leave the claim and escalate for a recorded
     security disposition. Do not guess.
4. **Close it with the fenced store call.** No operator command exposes this
   today. The only supported close is
   `resolve_uncertain_unban(request_id, claim_token, resolution)` in
   `crates/core/src/member_moderation_store.rs`, called under the member queue
   by an engineer with the exact `request_id` from the report and the row's
   `claim_token`. The token fences the close: a wrong, stale or already-closed
   claim errors with `lost uncertain scheduled-unban claim` and changes
   nothing. Keep the token out of logs, tickets and chat. A row with no token
   cannot be closed through this call and needs the recorded security
   disposition. Do not `UPDATE` or delete the row by hand: the call also
   decides between `done`, `pending` and `superseded` from the ban intents.
5. **Re-run `two-bot moderation preflight`.** The id must be gone from the
   `[running]` section. Exit 0 means CLEAR.

`--allow-owed` overrides the refusal but does not close a claim: the member can
stay banned.

## Override

Re-run the check with `--allow-owed`, or boot with
`TWO_ALLOW_OWED_RELEASES=1`. The override proceeds and is logged loudly
(`moderation_disable_override`) with the still-owed ids. Prefer completing
or explicitly cancelling the releases instead.
