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

Exit 0 is CLEAR **or an explicit override** (`--allow-owed` or inherited
`TWO_ALLOW_OWED_RELEASES=1`), 1 is REFUSED, and 2 means the state could not be
read. Verify the explicit CLEAR report or JSON `clear: true` and
`overridden: false`, not exit 0 alone. The check is read-only: it opens
`TWO_DATABASE_URL` (`DATABASE_URL` when unset) without migrating and never
contacts Discord. `TWO_DATABASE_TLS` applies as usual.

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
disable guard keeps refusing boot. A `pending` row still owes a release and
is eligible only when the trusted ban intent is accepted/current and a sweep
is actually wired and running. The domain exposes `run_due_unbans`, but this
runtime has no sweep caller or ticker on `main`; enabling moderation or
restarting alone does not drain pending rows. PR #398 supplies that pending
integration, which must be installed and verified before relying on it.
A `quarantined` import with a `dispatch_uncertain` marker can close that
**dispatch uncertainty** through the same steps, though the report lists it
with the pending ids. Clearing its fence does not reconcile the remaining
untrusted expiry obligation.

Recovery, in order:

1. **Confirm it is stranded.** Stop the worker, then list the claims
   (read-only; do not paste member ids into tickets or chat):

   ```sql
   SELECT request_id, guild_id, user_id, claimed_at
     FROM moderation_scheduled_unbans WHERE state = 'running' ORDER BY request_id;
   ```

   Re-read after the container is down to preserve the exact outstanding
   claim. Stopping the worker does not prove a dispatched DELETE stopped.
2. **Check the Discord ban list** for that member: Server Settings, Bans,
   and collect the member-unban audit evidence. These are supporting evidence,
   not sufficient proof on their own. Require authoritative evidence tied to
   the **exact guild, member, request and dispatched attempt** held by this
   claim before closing it. An older or manual unban for the same member is
   not this operation. If an audit entry cannot be correlated to this exact
   DELETE, it cannot justify `Completed`. The request/claim-token fence
   identifies the local row; the store does not authenticate remote evidence.
   A current banned or not-banned status, elapsed time and killing the worker
   do not prove the DELETE finished or cannot still land.
3. **Choose the outcome the exact-operation evidence proves.**
   - That dispatched unban provably finished: `Completed`. The row becomes
     `done`.
   - That DELETE provably cannot still land: `Void`. A trusted expiry returns
     to `pending` while its ban is still the member's current accepted ban,
     and becomes `superseded` once a newer accepted ban replaced it. A pending
     expiry is still owed; it requires the actually installed/running sweep
     described above, not merely a restart.
   - For an imported `quarantined` claim, `Void` leaves it **quarantined**,
     clears `dispatch_uncertain` and the claim token, and preserves the
     untrusted expiry as owed and non-executable. Separately reconcile that
     original expiry with authoritative intent/acceptance evidence and a
     recorded security disposition; never activate it through a hand-update
     or assume clearing dispatch uncertainty completes the release. See
     [the imported-schedule boundary](member-moderation.md#durability).
   - Neither is provable, or the evidence cannot identify the exact operation:
     leave the claim/fence and escalate for a recorded security disposition.
     Do not guess.
4. **Close it with the fenced store call.** No operator command on `main`
   exposes this today. Pull request #398 (open, not merged) adds
   `two-bot reconcile-member --guild <id> --resolve-unban --request <req> --claim <token> (--completed | --void) [--execute]`,
   a staging-guild-only command that is dry-run unless `--execute` is given
   and never prints the claim token. Once #398 merges, prefer it and drop the
   rest of this step; until then, the only supported close is
   `resolve_uncertain_unban(request_id, claim_token, resolution)` in
   `crates/core/src/member_moderation_store.rs`, called under the member queue
   by an engineer with the exact `request_id` from the report and the row's
   `claim_token`. The token fences the close: a wrong, stale or already-closed
   claim errors with `lost uncertain scheduled-unban claim` and changes
   nothing. Keep the token out of logs, tickets and chat. A row with no token
   cannot be closed through this call and needs the recorded security
   disposition. Do not `UPDATE` or delete the row by hand: the call also
   decides between `done`, `pending`, `superseded` and retained `quarantined`
   state from the claim and ban intents.
5. **Re-run preflight without either override.** Omit `--allow-owed` and
   unset the inherited override for this invocation:

   ```sh
   env -u TWO_ALLOW_OWED_RELEASES two-bot moderation preflight --json
   ```

   Verify JSON `clear: true` **and** `overridden: false` (or the explicit CLEAR
   text report without `--json`). Removal from `[running]` proves only that
   dispatch uncertainty was closed; `pending`/`quarantined` expiries,
   lockdowns and scheduled messages can still be owed. A remaining refusal
   requires their separate resolution before disabling. Exit 0 under an
   override is not CLEAR and cannot serve as recovery evidence.

`--allow-owed` overrides the refusal but does not close a claim: the member can
stay banned.

## Override

Re-run the check with `--allow-owed`, or boot with
`TWO_ALLOW_OWED_RELEASES=1`. The override proceeds and is logged loudly
(`moderation_disable_override`) with the still-owed ids. Prefer completing
or explicitly cancelling the releases instead.
