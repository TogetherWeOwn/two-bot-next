# Command restoration rehearsal (global + guild)

`docs/cutover.md` makes command restoration a cutover precondition: it must be
rehearsed for **both global and guild scopes**, permission-restore access must be
verified before any rename/removal (the bot token cannot write permissions), and
watch-window registry/permission changes must be reconciled rather than reset to
the frozen baseline. This page describes the dry-run harness that rehearses that
procedure. It does not replace the procedure in
[`cutover.md`](cutover.md#command-definitions-and-separate-guild-permission-recovery).

The harness is pure and in-memory: `two_bot_core::command_restoration` plus the
fixture suite `crates/core/tests/command_restoration_rehearsal.rs`. It needs no
Discord token, database or network, and it never writes to Discord. A
[`MockDiscord`](#mock-transport) transport stands in for the REST routes.

## What it models

| Cutover requirement | Harness element |
| --- | --- |
| Frozen baseline of definitions **and** separate guild permission overrides, including guild overrides on global commands | `ScopedRegistry` (one per scope: `CommandScope::Global` or `CommandScope::Guild { guild_id }`) |
| Diff the frozen baseline against the staged (reviewed Next) registry | `diff_scopes(baseline, staged, lineage)` returns classified `RegistryDrift` |
| Verify OAuth2 Bearer permission-restore access before any rename/removal | `verify_permission_restore_access` returns a named `AccessGap` on the first missing element |
| Rehearse watch-window drift | `simulate_watch_window(baseline, &[DriftEvent])` |
| Reconcile to an approved target, not an automatic reset | `reconcile_watch_window` returns a `ReconcileReport` |
| Apply definitions first, retain actual IDs, reapply complete per-command override arrays, read back against the target | `rehearse_scope` driving `MockDiscord` |

## Drift classes

`RegistryDrift::class()` labels each finding:

| Class | Meaning |
| --- | --- |
| `added` | Command present in the later registry, absent from the baseline. |
| `deleted` | Command present in the baseline, absent from the later registry. |
| `renamed` | A delete+add pair explained by **approved** rename lineage. Without lineage the pair stays `added` + `deleted`; the harness never infers a rename from shape similarity. |
| `definition_modified` | Same name, different wire shape; `field` names the first difference (`description`, `options`, `dm_permission`, or `wire`). |
| `default_changed` | `default_member_permissions` moved. Effective access moves with it even where no explicit override exists. |
| `override_added` | New explicit override row on a shared command. |
| `override_removed` | Explicit override row removed. `revoked_allow: true` marks a removed allow, which is a revocation. |
| `override_changed` | Same resource row, allow/deny flipped. |

Watch-window events (`DriftEvent`) cover every class the cutover procedure names:
`RevokedAllow`, `ChangedDefault`, `AddedCommand`, `DeletedCommand`,
`RenamedCommand` and `ChangedOverride`.

## Fail-closed access check

Permission writes need an existing authorized OAuth2 Bearer token with
`applications.commands.permissions.update`, held by a user with Manage Guild and
Manage Roles who can run the command and manage the affected resources. The
check reports the first missing element as a named gap:

| `AccessGap` | Cause |
| --- | --- |
| `BotTokenInsufficient` | The rehearsal was attempted with the bot token. This short-circuits every other check. |
| `MissingBearerScope` | No Bearer token with `applications.commands.permissions.update`. |
| `MissingManageGuild` / `MissingManageRoles` | The authorizing user lacks the guild permission. |
| `CannotRunCommand(name)` | The authorizing user may not run that command. |
| `CannotManageResources(name)` | The authorizing user may not manage that command's resources. |
| `UnmappedCommand(name)` | Reserved for the executor's command-ID mapping. The harness itself reports unmapped or ambiguous IDs through `frozen_reasons`. |

Any gap is **NO-GO**. Route it to manager/CISO provisioning review. A gap is never
permission to obtain, substitute or borrow another credential. `rehearse_scope`
records the gaps and stops before any reconciled PUT. The mock permission route
also refuses writes until `grant_bearer_access` is called, matching the real
route.

## Reconciliation rules

`reconcile_watch_window(baseline, staged, live, lineage, live_ids)` starts from
the staged registry and the final live snapshot, then:

- preserves legitimate window additions, deletions and renames instead of
  discarding them or recreating a deliberately deleted command;
- carries revocations and tightened defaults (`carried_revocations`,
  `carried_default_tightenings`). A removed allow is never reintroduced from the
  baseline, and current access is never broadened;
- emits the **complete** per-command override array for each command, because a
  per-command permission PUT replaces that command's overrides wholesale;
- records `(scope, command, live_id -> restored_id)` in `id_map`. Recreated
  commands get fresh server IDs on the mock, so the old/current/restored map is
  exercised rather than assumed;
- puts unsupported or ambiguous drift (for example two candidate rename
  targets) in `frozen_reasons`. A non-empty list means `is_go() == false`: keep
  commands frozen and escalate to the Director of Engineering.

## Mock transport

`MockDiscord::from_baseline` seeds one or more scopes. It supports `snapshot`
(GET), `put_definitions` (full-replacement PUT that assigns fresh IDs to
recreated names), `put_permissions` (per-command PUT, refused without Bearer
access) and `read_back`, which compares definitions, defaults and override tuples
against the approved reconciled target. Every call is appended to `log` so tests
can assert ordering, for example that no reconciled PUT happens after an access
gap. There is no batch permission route: the real batch endpoint is disabled and
is not a fallback.

## Running it

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib command_restoration
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test command_restoration_rehearsal
```

CI runs both through the workspace `cargo test` step. The fixture suite asserts:

- drift classes on guild and global fixtures (definition, override and default
  changes);
- each missing access element fails closed with its named gap, and the bot token
  fails first;
- the reconciled target preserves window additions, deletions and renames and
  never broadens access, while a naive baseline restore would undo them;
- a full rehearsal is GO on both scopes with provisioned access, and stops before
  any reconciled PUT without it.

## Limits

This rehearses the procedure; it is not the authorized REST/permission executor
and it is not cutover authorization. It does not cover application-level
default restoration (cutover step 4 still needs a separately authorized path),
real Discord writes, or credential provisioning. Never point it, or any
executor built from it, at production or staging guilds.
