# Command publish diff record (template)

One filled copy per publish per guild (staging now, production at cutover).
The staging voice-command inventory consumes this template unchanged: it
fills one copy and links it, it does not redesign it.

Offline-first: the record compares the compiled desired set
(`InteractionRouter::publish_set` under the publishing gates in
`docs/command-publish.md`) against the registry readout. Filling the
template never publishes, never touches permissions, and never needs a
Discord token beyond what the readout itself needs.

## Header (fill once per record)

- Date (UTC):
- Build revision (staging `/readyz` `build_revision`, or the exact head SHA):
- Guild ID (never the live guild without the rollout approval in
  `docs/command-publish.md`):
- Desired-set hash / current-registry hash (`two-bot commands diff` prints
  both SHA-256 hashes):
- Recorder:

## Schema

One row per command in the desired set, plus one row per removal (a remote
command absent from the desired set). Commands with no change since the
last record for the same guild may collapse to a single "unchanged since
<revision>" summary row instead of one row each.

| Command | Guild / global | Permission delta | Verified-by | Evidence link |
| --- | --- | --- | --- | --- |
| `TYPE/NAME` (for example `1/export`; `1` = slash command) | Guild `<id>` or Global | `none`, or `old -> new` (for example `ManageEvents -> Manage Guild`) naming the exact permission field that moved | Who or what confirmed it: `two-bot commands diff` readout, source read of `<file>:<line>`, exact CI job on the head SHA | Link to the readout, file blob, or check run that proves this row |

Column rules:

- **Command**: the `TYPE/NAME` form from the `two-bot commands diff`
  readout (`+` added, `-` removed, `~` changed). A removal row names the
  remote-only command and says where it went (disabled feature, legacy,
  dynamic/custom registry).
- **Guild / global**: exactly one of `Guild <snowflake>` or `Global`.
  The live-guild fence in `docs/command-publish.md` applies before any
  live read.
- **Permission delta**: `none`, or the old and new values of the exact
  field (`default_member_permissions`, per-guild override). Never write
  "permissions look fine".
- **Verified-by**: the method plus the revision it ran against. A
  source read cites the file; a readout cites the two hashes; a
  live-exercise cites the guild and the run. `not exercised` is an
  honest value when the check was read-only.
- **Evidence link**: one clickable proof per row. No proof, no row:
  mark it `missing` and say what would produce it.

## Worked example: V11 staging publish check, 2026-10-04 (read-only)

Source: the merged V11 import-map staging publish check evidence
(staging `/readyz` `build_revision 96ff8512`, `/health` ok, gateway and
database ready, no writes, no Discord token, live guild untouched).
Import-map validation rows from the same check (unknown-field tolerance,
invalid-value rejection, empty-map rules, oversize gate, duplicate
entries) are config-codec behavior pinned offline, not command-publish
surface, so they do not appear below; they stay covered by the offline
fixture suite.

- Date (UTC): 2026-10-04
- Build revision: `96ff8512` (staging `/readyz`, deploy run in progress
  at check time)
- Guild ID: staging guild (live guild never touched)
- Desired-set / current-registry hashes: not captured (live registry not
  probed: needs a Discord token, out of the read-only scope)
- Recorder: read-only staging probe, no writes

| Command | Guild / global | Permission delta | Verified-by | Evidence link |
| --- | --- | --- | --- | --- |
| `1/export` | Guild (staging) | none (Manage Server gate unchanged, fail-closed with ephemeral delivery) | Source read of the V11 export path plus `/readyz` revision match; not live-exercised | Check rows 1 and 11 of the 2026-10-04 V11 staging evidence |
| `1/import` | Guild (staging) | none (same Manage Server gate as `/export`; 20-line preview cap, confirm-before-write) | Source read of the V11 import preview/confirm path plus `/readyz` revision match; not live-exercised | Check rows 2 and 11 of the 2026-10-04 V11 staging evidence |
| (registry) V11 voice commands on the staging guild | Guild (staging) | none recorded (no publish performed) | Read-only probe: staging vars set only automations/announcements (no voice gate), boot publish off by default, so the compiled commands are absent from the live registry by configuration, not by drift | Check row 12 of the 2026-10-04 V11 staging evidence (gapped: compiled shapes exist, live registry unprobed) |

Reading this example: both V11 commands were present in the staging
build with their permission gate intact, and neither was published to
the staging guild because the voice gate was off. That is a
configuration state, not command drift, and the next inventory run
reuses these exact rows as its baseline.
