# Moderation/automation disable guard

Disabling moderation (`TWO_MODERATION` unset) or automations (`TWO_AUTOMATIONS`
unset) while Postgres still owes releases strands members in bans and
channels in lockdown silently. The disable guard refuses that boot.

## What it checks

When either gate is off at boot, the container reads the owed state from
Postgres — never Discord — before starting the gateway:

- `moderation_scheduled_unbans` rows outside the terminal states
  (`done`, `superseded`, `cancelled`) are owed tempban releases;
- every `moderation_lockdowns` row is an active lockdown holding recovery
  state;
- every enabled `scheduled_messages` row would stop firing (read only when
  the table exists).

Any owed release refuses boot (exit 1, `moderation_disable_refused`), naming
the outstanding request, channel, and schedule ids. While both gates are on,
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

## Override

Re-run the check with `--allow-owed`, or boot with
`TWO_ALLOW_OWED_RELEASES=1`. The override proceeds and is logged loudly
(`moderation_disable_override`) with the still-owed ids. Prefer completing
or explicitly cancelling the releases instead.
