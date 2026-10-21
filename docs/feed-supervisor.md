# Feed supervisor operator guide

How to run feed delivery (`/feed-add`, `/feed-list`, `/feed-remove` plus the
managed poller). This is the operator view. The port record
(`docs/feeds-port.md`) remains the domain/store contract; this guide does not
restate it.

Out of scope: code changes and dashboard queries. Cadence hot-reload is
shipped behavior (see [Fetch cadence](#fetch-cadence)): a stored interval
change reschedules the next run without a restart.

## Source configuration

Three slash commands, all guild-only, all requiring ManageGuild (`32`), none
available in DMs (`docs/commands.md`):

| Command | Options | Notes |
| --- | --- | --- |
| `/feed-add` | `kind` (required choice: `"rss"`, `"youtube"`, `"twitch"`), `source` (required string, max 2048 chars) | Posts into the invoking channel. At most 2,048 input bytes total. |
| `/feed-list` | none | Ephemeral text; each source capped at 128 UTF-16 units, shrinking adaptively at capacity (scheme-stripped head…tail so same-host relays stay distinguishable) so every relay id stays discoverable within the 2,000-unit message cap (`crates/core/src/feeds.rs`). |
| `/feed-remove` | `id` (required string, max 128 chars) | Replies `Feed relay removed.`, `No feed relay with that id.` (`crates/core/src/feeds.rs:354-360`) or, while a delivery is still pending recovery, `Feed relay kept: unresolved deliveries are still pending recovery.` (`crates/core/src/feeds.rs:362-366`). Guild-scoped delete. |

Source rules (`crates/core/src/feeds.rs:168-184`, `docs/feeds-port.md`):

- RSS and Twitch sources are HTTPS feed URLs. New HTTPS sources must use
  port 443; existing enabled relays on other ports stay stored but fail
  validation whenever polling runs. Disabled relays are not polled.
- YouTube also accepts a 20–32-character channel ID (`[A-Za-z0-9_-]`),
  expanded to `https://www.youtube.com/feeds/videos.xml?channel_id=<id>`.
  Twitch is **not** a Helix API integration and needs no Twitch credential.
- Adding a relay does not fetch the source. At most 25 feed relays per
  guild (disabled relays count; `docs/runbook.md:1312`).
- Removal cascades settled delivery history (`feed_deliveries` rows go with
  the relay). While any delivery is still pending recovery, removal is
  refused and keeps the relay and its ledger; let recovery settle it, then
  remove again (`docs/feeds-port.md`). Do not remove and re-add a relay to
  "fix" it unless resetting its delivery history is intended.

Gates (both must hold or the poller parks):

- `TWO_ANNOUNCEMENTS=1` — exact `1` enables; anything else is off.
  Default off (`crates/core/src/feature_commands.rs:346-383`).
- Boot identity must permit the `announcements` capability
  (`feed_jobs::register_fenced`, the live-identity fence). A mismatched or
  unparseable identity parks the job with
  `feed poller parked: live activation refused`
  (`crates/bot/src/feed_jobs.rs:304-317`).
- `TWO_FEED_POLL_SECONDS` — default `300`, valid range 60–86400. An
  invalid value parks the poller with
  `feed poller parked: invalid feature configuration`
  (`crates/bot/src/feed_jobs.rs:274-284`); the gate error text is
  `TWO_FEED_POLL_SECONDS must be an integer between 60 and 86400`
  (`crates/core/src/feature_commands.rs:354-357`).

Per-feed fetch contract (what a "source is down" means): exact User-Agent
`Owen/1.0 (+https://two.gg)`, 15-second total deadline across
DNS/redirects/body parsing, 2 MB decompressed ceiling, at most 3 same-host
HTTPS redirect hops, at most 20 new posts per feed per pass
(`docs/feeds-port.md`).

## Fetch cadence

As coded (`crates/bot/src/feed_jobs.rs:33-41,227-267`):

- The `feeds` job registers in the website/community job supervisor and runs
  inline — no detached tasks, no second dispatcher.
- Supervisor cadence is seeded from the boot `TWO_FEED_POLL_SECONDS` value
  (or the already-published stored value at construction) and then follows
  the stored live interval through `Job::cadence_source`
  (`crates/bot/src/feed_jobs.rs:235-265`, `crates/bot/src/jobs.rs:193-252`).
  The per-tick `FeedPollSchedule` gate re-reads the same stored interval
  before every tick (`crates/bot/src/feed_jobs.rs:274-280`,
  `crates/core/src/feeds.rs:1320-1333`). A stored change reschedules the
  pending run to last-run + new interval in both directions: a decrease
  below the boot cadence fires at last-run + new interval without waiting
  for the pending boot-cadence slot, and an increase pushes the pending
  tick out a full new interval. The effective interval is the stored value,
  never rounded to a boot multiple. An unchanged stored value leaves the
  pending slot untouched (no-op); an out-of-range stored value falls back
  to the boot interval end to end (see below), rescheduling the pending run
  when the current schedule is not already boot. Overlapping passes stay
  suppressed: the
  gate refuses a tick while a pass is running, and the supervisor skips a
  tick while the previous pass is still active.
- Stored live values are not boot configuration. An absent, unparsable, or
  out-of-range stored value falls back to the boot interval, and a deleted
  stored row hands the interval back to the boot value
  (`crates/bot/src/feed_jobs.rs:201-226`). An invalid boot
  `TWO_FEED_POLL_SECONDS` still refuses registration (`configuration`) and
  parks the poller even when a live value is already published
  (`crates/bot/src/feed_jobs.rs:253-259`). Hot-reload regressions:
  `live_poll_seconds_prefers_the_stored_value`,
  `schedule_gate_observes_a_stored_increase_without_restart`, and
  `supervisor_applies_a_hot_reloaded_feed_interval_without_restart`
  (`crates/bot/src/feed_jobs_tests.rs:269-437`).
- Immediate first pass, overlapping passes suppressed, no startup jitter
  (`startup_jitter: Duration::ZERO` for this job).
- Pass budget 100 s under the supervisor's 120 s job timeout. Each relay
  gets a 30 s deadline covering discovery, fetch/parse, claims, sends and
  audit. A relay starts only if a full 30 s slot remains; otherwise the
  pass ends `Timeout` so a boundary relay is never starved on a residual
  slot.
- A job-owned cursor advances **before** each relay starts in stable
  `(created_at, id)` order and resumes at its successor next pass.
  Deleting/disabling the cursor row does not reset progress; a process
  restart resets the in-memory cursor.
- Recovery gets at most 20 expired ledger rows per feed per pass inside a
  12 s phase budget; each history read needs 6 s of remaining budget for
  the executor's 5 s call deadline. A budget refusal stops recovery without
  releasing or reposting pending rows.

## Failure signals and triage

Job error classes (`crates/core/src/metrics.rs:135-155`):
`database`, `rest`, `configuration`, `timeout`, `panic`, `feed`,
`recovery_required`. Visible on the `/readyz` status surface as
`last_error_class` with `consecutive_failures`
(`crates/bot/src/jobs.rs:24-31`).

| Signal | Meaning | Triage |
| --- | --- | --- |
| `feed poller parked: invalid feature configuration` | Bad `TWO_FEED_POLL_SECONDS` (or gate env) at boot | Fix the env value to an integer in 60–86400 and restart. |
| `feed poller parked: live activation refused` | Boot identity does not permit `announcements` | Check the boot guild/application pair and token; the poller posts under the token's identity. |
| `feed poll deadline exceeded` (`feed_id`) | Relay hit its 30 s slot | Check the source's availability and recent fetch status; the cursor already advanced, so the next pass continues after this relay. No manual cursor change. |
| `feed recovery deadline exceeded` (`feed_id`) | Recovery phase hit its 12 s budget | Pending rows stay pending for nonce reconciliation next pass. Do not replay manually. |
| `feed items skipped: item URL has mass-mention text or masked-link syntax` (relay id + count) | Relay polls cleanly but items filtered before send | A source whose links look like mass mentions or masked links (`](`) never posts by design. Change the source feed, not the bot. |
| `feed` error class on a pass | Fetch/parse failure for a relay | Invalid items and feed failures do not stop later items/feeds. Check source availability; audit row `poll_failed` / `fetch_or_parse`. |
| `recovery_required` error class / audit reason | Send accepted-unknown, completion error, or ambiguous history evidence | Preserve the feed cursor and pending recovery state; follow the recovery path before any manual replay (`docs/runbook.md:242`). Missing in a bounded 100-message search is not authoritative absence — never repost to "fix" it. |
| `timeout` error class | Pass or relay budget exhausted | Same as the deadline rows above; repeated timeouts on one relay point at its source, not the supervisor. |
| `database` / `rest` classes | Pool or Discord REST failure | Treat as platform, not feed config; see the operations runbook. |

Audit trail: every pass writes fixed-class reasons (`poll_result`,
`poll_failed`/`fetch_or_parse`, `item_failed`/`invalid_item` /
`send_rejected`, `recovery_required` with bounded reason strings) via the
shared `announcements_audit_log` coexistence path — raw HTTP, source and
database errors are never logged (`crates/bot/src/feed_jobs.rs:383-412,549,636-657`).
Check the configured feed source's availability and recent fetch status
through authorized diagnostics; do not copy response bodies
(`docs/runbook.md:241`).

## Read-only guarantees

- Inspect first. Poller state (cursor, pending claims) is the delivery
  record: restarting to "unstick" a relay resets the in-memory cursor and
  replays from the head; a `recovery_required` row reposts as a duplicate
  if replayed by hand.
- Never remove and re-add a relay as remediation (delivery history
  cascades). Never edit stored sources in place; there is no reviewed
  in-place remediation path.
- History reads are bounded (100 messages) and non-authoritative on a
  miss. Unknown acceptance stays pending by design.
- Tests and diagnostics never touch staging or production data: DB tests
  run only against disposable test databases
  (`TWO_TEST_DATABASE_URL=...agent_test@agent-testdb...`, ignored by
  default, executed by the `feeds db check` CI job), and the `two-bot
  commands diff` / `preflight` surfaces are read-only unless `publish`
  receives an explicit `--apply` (`crates/bot/src/commands_cli.rs:19-30`,
  `crates/bot/src/preflight.rs:32-42`).

Operator CLI reference (pasted from the `--help` sources; each `--help`
path prints its `USAGE` constant):

```text
two-bot commands diff [--guild-id ID] [--application-id ID] [--allow-live-guild]
two-bot commands publish [--apply] [--guild-id ID] [--application-id ID] [--allow-live-guild]
    Fetch and compare the guild command list against the compiled, feature-gated
    builtins. Both commands are read-only unless publish receives --apply.
    Env: DISCORD_TOKEN, GUILD_ID, DISCORD_APPLICATION_ID; feature gates as on boot
    (TWO_VOICE=1 adds the voice set; /templateassistant stays unpublished until its slice lands).
    Live Discord targets additionally require TWO_DATABASE_URL (or DATABASE_URL
    when unset), the shared send-admission Postgres. No database is opened for
    loopback DISCORD_API_BASE fixtures, and no gateway is started.
    See docs/command-publish.md before applying a cutover.
```

(`crates/bot/src/commands_cli.rs:19-30`; `--help` prints it at
`commands_cli.rs:175-176`.)

```text
two-bot preflight [--json] [--level-role-ids CSV]
    Read-only Discord REST credential, intent, role hierarchy and configured
    channel checks. No gateway or Discord permission changes. Live requests
    require the runtime TWO_DATABASE_URL admission authority.
    Env: DISCORD_TOKEN, TWO_DATABASE_URL, GUILD_ID, feature/channel configuration.
    --level-role-ids: exported level_role_rewards role IDs (empty CSV means
    no rewards). Required when onboarding permits level-role writes.
    Exit 0: PASS/WARN only; 1: FAIL; 2: invalid CLI/configuration.
    See docs/preflight.md for coverage and interpretation.
```

(`crates/bot/src/preflight.rs:32-42`.)

```text
two-bot erase-member --guild <id> --user <id> [--execute]
    Dry run by default: print matching row counts, write nothing.
    --execute deletes in one transaction; requires TWO_ERASURE_ACTOR.
    Env: TWO_DATABASE_URL (required). See docs/privacy.md.
```

(`crates/bot/src/erasure_cli.rs:8-13`. Shown as the dry-run-first pattern:
feed work has no equivalent `--execute`; there is no in-place source edit.)

No other flags are documented here. If a flag is not in this guide, it is
not part of feed operation.

## Worked example 1: add an RSS relay and confirm polling

1. In the target channel, as a member with ManageGuild, run
   `/feed-add kind:rss source:https://example.com/feed.xml`.
   Expect the relay id in the confirmation reply; copy it.
2. Run `/feed-list` in the same guild. Expect a line
   `` `<id>` rss → <#<channel>> https://example.com/feed.xml ``
   (sources shrink to fit at capacity; every relay id stays listed).
3. Wait one poll interval (default 300 s; configured via
   `TWO_FEED_POLL_SECONDS`). Adding a relay does not fetch the source —
   the first delivery happens on the next pass.
4. Confirm: new items post in the invoking channel (up to 20 per pass,
   oldest first, bare canonical URLs for link previews). Confirm via
   `/feed-list` and channel posts, not via database reads.
5. If nothing posts after two intervals: check the supervisor status for
   the `feeds` job (`last_error_class`), then match
   [Failure signals](#failure-signals-and-triage). A clean poll with no
   posts plus the `feed items skipped` log line means the source's URLs
   are filtered by design — pick a different source.

## Worked example 2: triage a `recovery_required` relay

1. Note the `feed_id` and the audit reason (`history_unconfirmed`,
   `send_unconfirmed`, or `completion_unconfirmed`;
   `crates/bot/src/feed_jobs.rs:470-578`). Do not delete, disable, or
   re-add the relay: that resets delivery history and risks duplicates.
2. Check whether the message actually landed: look in the relay's channel
   for the item (exact 24-character nonce match is internal; channel
   presence is the operator check). A bounded history miss is not proof
   of absence — if the message is not visibly there, still do not repost.
3. Leave the pending claim alone. The next passes retry reconciliation
   automatically (up to 20 expired rows per feed per pass, oldest leases
   first); a completed history attempt renews or completes the fenced
   claim.
4. If the class persists across passes with `rest` errors, treat it as a
   Discord REST problem (operations runbook), not feed config. If it
   persists with clean REST, the claim stays pending by design rather
   than risking a duplicate — escalate with the `feed_id`, the audit
   reason, and the pass timestamps.

## Verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test feeds --test feeds_connector --test reference_docs --locked
```

Results (2026-10-17 run, cache wrapper, exact command above):

```text
test result: ok. 30 passed; 0 failed; 0 ignored (feeds)
test result: ok. 28 passed; 0 failed; 0 ignored (feeds_connector)
test result: ok. 7 passed; 0 failed; 1 ignored (reference_docs;
  ignored test regenerates committed docs and never runs normally)
```

```sh
cargo fmt --all -- --check  # exit 0, clean
```

Committed tests behind this guide (all offline except where noted):

- `crates/core/tests/feeds.rs` — parsing, budgets, claim arbitration.
- `crates/core/tests/feeds_connector.rs` — pinned-request/body policy
  without sockets.
- `crates/core/tests/feeds_store.rs` — migration coexistence, fencing
  (ignored by default; `feeds db check` CI job runs them against its
  disposable Postgres service).
- `crates/bot/src/feed_jobs_tests.rs` — managed-poller journeys on REST
  doubles and disposable databases, plus non-ignored deadline tests.
- `crates/core/tests/reference_docs.rs` — pins `docs/configuration.md`
  catalog rows cited above.

`--help` USAGE blocks above are pasted verbatim from the cited source
constants (the exact strings each `--help` path prints). The `two-bot`
binary was not built in this docs run, so no binary `--help` transcript
is claimed. Slash-command options are pasted from `docs/commands.md`
(rendered from the compiled registry) and the option decoders at
`crates/bot/src/command_runtime.rs:2124-2160`.
