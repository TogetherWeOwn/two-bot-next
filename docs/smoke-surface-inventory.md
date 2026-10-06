# Slash-command surface inventory (offline smoke input)

Offline only: no staging call, no guild publish, no database. This is the
input to the live-guild smoke pass, not the pass itself. The stager takes
this list, runs the five covered commands against staging, and records
verdicts in `docs/smoke-run-record.md`.

## Source

- Registry: `InteractionRouter::publish_set` with all feature gates on
  (scorecard, automations, announcements, moderation), no custom rows.
- Rendered reference: `docs/commands.md` — 28 built-ins, registry bounds,
  DMs false on every row (guild-only, legacy `setDMPermission(false)`).
- Merge order (first definition wins, Discord 100-command ceiling):
  core → scorecard → automations → announcements → moderation.

Counts: 3 core + 1 scorecard + 8 automations + 7 announcements +
9 moderation = **28 built-ins**. Machine-readable mirror:
`crates/core/tests/fixtures/smoke_surface_inventory.json`, checked by
`crates/core/tests/smoke_surface_inventory.rs` (registry shape in publish
order, exactly-five-covered gate, handler routing for the five).

## Covered: the smoke five (one per routing family)

Each row links the exact expected reply in
`docs/smoke-expected-responses.md` and its run-record row in
`docs/smoke-run-record.md`. Routing pins live in
`crates/discord/tests/top5_reply_fixtures.rs`.

| # | Command | Family | Gate / permission | Expected response |
| --- | --- | --- | --- | --- |
| 1 | `/rank` | core (always on) | Everyone | Expected-response table, `/rank` happy-path shape; run-record row 1 |
| 2 | `/leaderboard` | core (always on) | Everyone | Expected-response table, `/leaderboard` happy-path shape; run-record row 2 |
| 3 | `/rsvp` | announcements (open) | Everyone | Expected-response table, `/rsvp` happy-path echo; run-record row 3 |
| 4 | `/lfg` | announcements (gated) | ManageEvents | Expected-response table, `/lfg` creation + signup/leave shapes; run-record row 4 |
| 5 | `/ban` | moderation (gated) | BanMembers | Expected-response table, `/ban` routing + refusal copy; run-record row 5 |

Denied-path probes for the gated three (gate off, permission missing) are
rows 6–8 of the run-record sheet, using the shared denied copy.

## Deferred: 23 commands with reasons

Deferred means out of the read-only smoke, not untested: every row below
is published by the same registry the unit test pins, and most share a
handler family with a covered command.

| Command | Family | Deferral reason |
| --- | --- | --- |
| `/help` | core | Next-only discovery command with no legacy counterpart; answers from the live publish set with no store or network effect, so offline router and renderer tests pin it instead of the live smoke five. |
| `/attendance` | scorecard | Needs a seeded event-occurrence plus a verified-human fixture; outside the one-per-family smoke budget. |
| `/command` | automations | Mutating admin CRUD that creates live commands; the read-only smoke excludes state writes. |
| `/command-remove` | automations | Destructive admin op; excluded from the read-only smoke. |
| `/command-list` | automations | Needs DB-backed custom rows, which the no-DB rule excludes; the list is empty in the smoke guild. |
| `/schedule` | automations | Effect fires on a wall-clock delay the offline smoke cannot observe; needs a timed live-guild pass. |
| `/schedule-remove` | automations | Destructive and needs a live scheduled-message id; sequenced after a schedule create, not standalone. |
| `/schedule-list` | automations | Needs seeded scheduled rows (DB); the no-DB rule excludes it. |
| `/sticky` | automations | Posts into a live channel (channel mutation); excluded from the read-only smoke. |
| `/sticky-remove` | automations | Channel-state mutation; excluded from the read-only smoke. |
| `/rsvp-attendance` | announcements | Needs seeded RSVPs on a live scheduled event; the announcements-open family is covered by `/rsvp`. |
| `/lfg-close` | announcements | Needs a live LFG post id; sequenced after the `/lfg` create step in the live pass, not standalone. |
| `/feed-add` | announcements | Triggers external fetch plus channel relay writes; the offline rule excludes network effects. |
| `/feed-remove` | announcements | Destructive and needs a live feed id; sequenced after a feed-add, not standalone. |
| `/feed-list` | announcements | Needs seeded feed relays (DB); the no-DB rule excludes it. |
| `/tempban` | moderation | Same handler family as `/ban` (covered); the live membership effect is excluded from the read-only smoke. |
| `/kick` | moderation | Same handler family as `/ban` (covered); the live membership effect is excluded from the read-only smoke. |
| `/timeout` | moderation | Same handler family as `/ban` (covered); the live membership effect is excluded from the read-only smoke. |
| `/warn` | moderation | Same handler family as `/ban` (covered); writes a live audit record, excluded from the read-only smoke. |
| `/purge` | moderation | Deletes live messages; destructive, excluded from the read-only smoke. |
| `/slowmode` | moderation | Channel-state mutation; excluded from the read-only smoke. |
| `/lockdown` | moderation | Channel-state mutation; excluded from the read-only smoke. |
| `/unlock` | moderation | Channel-state mutation; excluded from the read-only smoke. |

## How the live-guild pass uses this inventory

1. Run the five covered commands per `docs/smoke-run-record.md` rows 1–5
   and compare against `docs/smoke-expected-responses.md`.
2. Run the denied-path probes (run-record rows 6–8) for `/rsvp`, `/lfg`,
   `/ban` with the gate off and the permission missing.
3. Add deferred commands to the live pass only when their reason is
   resolved (seeded fixture, timed window, or explicit approval for a
   state-changing probe) — never expand the read-only smoke silently.
