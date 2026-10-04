# Top-5 smoke run-record sheet (offline form)

Blank form the stager fills in during the live-guild pass. Offline only: no
staging guild, no live Discord, no secrets, no network, no database. Expected
values live in `docs/smoke-expected-responses.md` and are not restated here —
copy only the verdict shape into the rows below, never member data, tokens,
or guild internals.

## Environment header

| Field | Value |
| --- | --- |
| Bot revision (SHA) | _fill in_ |
| Staging guild | _fill in (name only, no IDs)_ |
| Run date/time (UTC) | _fill in_ |
| Stager | _fill in_ |
| Feature gates (automations / announcements / moderation) | on / on / on (circle actual) |
| Tester role (admin / moderator / plain member) | _fill in_ |

## Command rows

For each row: run the input, compare the reply against the expected-response
table section named in Expected, write what was seen in Observed (shape
tokens only, e.g. `rank line matched`, `ephemeral RSVP saved: going`),
mark Pass/Fail, and link evidence (log line, screenshot, or message link).

| # | Command | Input | Expected (`smoke-expected-responses.md`) | Observed | Pass/Fail | Evidence link |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | `/rank` | `/rank` (own profile) | Expected-response table, `/rank` happy-path shape | | | |
| 2 | `/leaderboard` | `/leaderboard` | Expected-response table, `/leaderboard` happy-path shape | | | |
| 3 | `/rsvp` | One status each: `going`, `interested`, `declined` | Expected-response table, `/rsvp` happy-path echo | | | |
| 4 | `/lfg` | Create post, then signup, then leave | Expected-response table, `/lfg` creation + signup/leave shapes | | | |
| 5 | `/ban` | Probe per read-only moderation procedure (no live effect without approval) | Expected-response table, `/ban` routing + refusal copy | | | |
| 6 | `/rsvp` denied | Gate off (announcements disabled) | Shared denied copy: announcements-disabled | | | |
| 7 | `/lfg` denied | Member without Manage Events | Shared denied copy: missing Manage Events | | | |
| 8 | `/ban` denied | Member without Ban Members; then moderation gate off | Shared denied copy: ban-permission refusal, then moderation-disabled | | | |

## Sign-off

| Field | Value |
| --- | --- |
| Stager signature (name) | _fill in_ |
| Date/time (UTC) | _fill in_ |
| Verdict (all pass / fails noted) | _fill in_ |
| Follow-up card for any Fail row | _fill in_ |

## Sources

- Expected responses: `docs/smoke-expected-responses.md`.
- Reply lifecycle and visibility rules: `docs/interaction-replies.md`.
