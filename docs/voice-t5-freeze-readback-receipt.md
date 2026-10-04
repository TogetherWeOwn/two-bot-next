# T5 freeze read-back receipt for the voice window (rollback freeze)

Offline definition of the frozen target-plus-read-back pair that trigger T5
(`docs/voice-cutover-rollback-triggers.md`, Registry/permission drift) is
walked against. No code, no staging writes, never production. A reviewer
replays T5 from the record alone (§7).

## 1. What T5 decides

T5 fires when final live command definitions or guild permissions at
rollback freeze (`T_r`) differ from the reconciled target with no approved
watch-window edit covering the drift, or when any role allow revoked during
the watch cannot be mapped to the legacy-compatible target. Response: keep
commands frozen with a named moderator/data-lead disposition; unresolved
drift goes to the Director of Engineering.

This is a **freeze comparison**, not a publication check. The separate
staging-publication question (a few automation/announcement commands absent
from the staging guild command list, owned by the staging smoke) asks
whether the staging guild publishes what the binary defines. T5 asks whether
the frozen live registry at `T_r` matches the approved reconciled target.

## 2. Live snapshot at `T_r` (what is captured)

After freezing all registry/permission writers (administrator edits and
automatic command sync included), the authorized executor captures, through
the separately authorized REST tool:

**Definitions** (raw GET snapshots kept; PUT payloads validated separately,
response-only fields filtered):

```text
GET /applications/{application.id}/commands?with_localizations=true
GET /applications/{application.id}/guilds/{guild.id}/commands?with_localizations=true
```

Every scope (global and guild) and every type (slash, user, message). A
definition GET does **not** snapshot role/user/channel overrides.

**Permissions** (separate snapshot; deleting or renaming a command
permanently deletes its permissions, so this is captured before any
overwrite):

```text
GET /applications/{application.id}/guilds/{guild.id}/commands/permissions
GET /applications/{application.id}/guilds/{guild.id}/commands/{command.id}/permissions
```

The first GET captures all returned permission objects, including
application-ID defaults for commands without explicit overrides; the second
supports per-command read-back. Retain role/user/channel IDs, types and
allow/deny values, including the `@everyone` and All-Channels markers, plus
whether each command is synced to defaults or carries explicit overrides.

**Voice access-controls row** (the guild's `/access` state, read from the
database by the executor, never from Discord):

- `room_creation_enabled` (global creation kill-switch),
- `required_role` (optional guild-wide role),
- `command_roles` (per-command allowed roles; a present-but-empty list
  denies every non-admin — fail closed).

The row must pass `validate_access_controls` (exact-lowercase command names
against the restrictable set, no zero role IDs). Database journals do **not**
capture registry or permission edits made directly in Discord, so the Discord
snapshot above stays authoritative for drift.

**Fence proof:** writers fenced before capture, capture timestamp (`T_r` in
UTC), capturer, tool version. If writers cannot be fenced or the capture is
inconsistent, keep commands frozen and do not overwrite the registry.

Source: `docs/cutover.md` ("Command definitions and separate guild
permission recovery", recovery order steps 1–5).

## 3. Reconciled target (what it is compared against)

The target is **approved**, never an automatic reset to the pre-swap
baseline. For a rollback it reconciles four inputs: the pre-swap baseline,
the post-swap receipts, the final live snapshot (§2), and the approved
watch edits. Retain the approved target and its mappings before any
mutation.

For the voice window the target pins:

1. **T0 command surface** (`docs/t0-acceptance-smoke-contract.json`, schema
   version 1): 27 built-ins with all gates on, in legacy publish order,
   guild-only throughout, plus the permission-bit table and the top-five
   routing sample. State the fixture source revision and the reviewed head
   actually compared; drift between them is reconciled, never assumed away.
2. **Voice published set** for `TWO_VOICE=1`
   (`voice_command_set` when the gate is enabled, empty otherwise): 17
   definitions at the reviewed head — `create`, `setup`, `ping`, `invite`,
   `textchannels`, `access`, `reclaim`, `transfer`, `logging`, `export`,
   `import`, `position`, `group`, `inheritpermissions`, `defaultlimit`,
   `alwaysprivate`, `kick` — merged first-wins into the guild registry by
   `InteractionRouter::publish_set` (`RouterGates::voice`; the published
   names and permissions are pinned by the `voice` section of
   `crates/core/tests/fixtures/staging_published_commands.json`).
   Admin shapes carry Manage Channels, except `export`/`import` which carry
   Manage Guild; member shapes (`setup`, `ping`, `invite`, `reclaim`,
   `transfer`, `kick`) carry no default permission gate. Plus the
   `templateassistant` command (Manage Guild, one required `request` option)
   only when **both** the voice gate and the assistant endpoint gate are on.
3. **Restrictable set** (`VOICE_COMMANDS`, 26 names): the exact-lowercase
   names `/access restrict` may name. This set and the published set differ
   by design (restriction names include owner-control and utility names that
   are not separately published slash commands; the `/access` command itself
   is published but not restrictable). Pin both; never assert their equality.
4. **Access-controls target**: creation flag, required role, and the full
   command-roles map after carrying current restrictions and revocations
   forward. Removing an allow entry is a revocation: never reintroduce it
   from the baseline, and never convert inherited defaults into explicit
   per-command overrides.
5. **Approved watch edits**: each with scope, time (UTC), approver, and the
   exact definition/permission delta it covers. Only listed edits cover
   drift.

Comparison is on **effective access** — definition defaults, role/user/
channel precedence and inheritance — not a naive union of permission
arrays. Map old/current command identities by application, scope, guild,
type and approved name/rename lineage, with complete old/current/restored
ID maps from the apply receipts (never assume recreated commands reuse old
IDs). An unsupported definition or ambiguous map is NO-GO, not permission to
discard it or recreate a deliberately deleted command.

Source: `docs/cutover.md` recovery order steps 2–5; `docs/voice-access-core.md`
(gate semantics and `/access`); T0 contract `docs/t0-acceptance-smoke-contract.md`.

## 4. Where the pair is recorded

| Artifact | Location | Content |
|---|---|---|
| Raw snapshots and validated PUT payloads (carry real IDs) | Restricted backup/evidence location, hashes pinned on the cutover card | Full GET bodies, per-command permission objects, ID maps |
| B4 manifest Registry row | Cutover card | Snapshot hashes, `T_f`/`T_0`/`T_r` times, reconciled-target pointer, read-back receipt pointer |
| Public replay record (§6 table) | This receipt (follow-up revision) | Names, counts, hashes, verdicts, disposition names — no IDs, no secrets, no private hosts |

Raw command payloads, member data, dumps and logs never go in public GitHub
or issue comments. The public record carries only the comparison outcome a
reviewer needs to replay the verdict.

## 5. Who dispositions drift

- Each unexplained drift gets a **named moderator + data-lead disposition**
  recorded against the receipt row (accept with rationale, or map to an
  approved watch edit).
- Unresolved compatibility or access changes keep commands frozen and go to
  the **Director of Engineering** as a decision brief. Never silently choose
  availability over access correctness, and never broaden current access to
  clear a mismatch.

## 6. Receipt table (filled at `T_r`, one row per command)

| Command (scope/type) | Live definition hash | Target definition hash | Live permission tuples (resource/type/allow + defaults/sync state) | Target permission tuples | Covering watch edit (or —) | Disposition (names) | Verdict |
|---|---|---|---|---|---|---|---|
| e.g. `kick` (guild/slash) | `sha256:…` | `sha256:…` | 3 explicit overrides, synced=false, effective=`…` | same | — | moderator + data lead, date | MATCH / DRIFT-FIRE |
| `voice_access_controls` row | creation=`…` required=`…` roles=`…` | same | — | — | — | names, date | MATCH / DRIFT-FIRE |

Close the receipt with: zero unexplained mismatches (before GO or before
reopening), the `T_r` timestamp, and the Dispatcher line (who captured, who
approved the target, who dispositions each DRIFT row).

Verdict rule: **NO-FIRE** when every live tuple maps to the reconciled
target with approved-edit coverage and revocations preserved; **FIRE** on any
uncovered definition/permission drift, any unmapped revoked allow, any
inconsistent capture, or any unresolved default/sync mismatch.

## 7. Reviewer replay (offline, ~15 min, record only)

1. Open the filled §6 table and the B4 Registry row: confirm `T_r` is
   recorded and all four snapshot classes (§2) are present with hashes.
2. Open the reconciled target: confirm the T0 fixture revision, the voice
   published set at the reviewed head (17 names), the restrictable set (26
   names), the access-controls target, and the watch-edit list.
3. Spot-check two rows (one admin-gated, one member shape): recompute the
   definition hash from the pinned source and confirm the permission tuples
   match the target, including defaults/sync state.
4. For every DRIFT row: confirm a covering watch edit or a named
   moderator/data-lead disposition; confirm no revoked allow was
   reintroduced from the baseline.
5. Confirm the verdict follows the §6 rule. Expected result: agree, or cite
   the receipt line that changes the verdict.

## 8. Limits

- Offline definition only: no `/readyz`, `/metrics`, container-log, database
  or Discord contact was used to author it; live halves rest on the
  executor's restricted snapshots at `T_r`.
- Voice sets drift with source: repin the published/restrictable lists to
  the window's reviewed head. This revision pins them at `96ff8512`
  (`origin/main` at authoring): 17 `voice_commands()` definitions, 26
  `VOICE_COMMANDS` names, T0 fixture at source revision `27d22b42`.
- Read-back compares against the **approved reconciled target**, not merely
  the frozen pre-cutover baseline; writers stay fenced through read-back,
  and any concurrent change forces recapture and reconciliation.
