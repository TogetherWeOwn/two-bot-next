# Expected-joins TTL/source acceptance

`crates/core/tests/expected_joins_acceptance.rs` pins the public
`two_bot_core::expected_joins` decision API (`crates/core/src/expected_joins.rs`).
It is pure and offline: no gateway, no SQL, no Discord client. The clock is
injectable (`ExpectedJoins::with_clock`) so TTL boundaries are deterministic.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test expected_joins_acceptance
```

CI runs it with the other integration targets (`cargo test --workspace --test '*'`).

Contract: one-click join attribution notes — 30 s TTL, `web:one_click`
source, guild+member keying; dying between the add call and the gateway event
falls back to the honest `unknown` the invite tracker already reports
(`consume` returning `None` means attribution proceeds by invite diff).

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | A note taken before the add call is consumed by the matching join within the 30 s TTL with source `web:one_click`; consuming clears the note (no double attribution) | `note_before_add_is_consumed_by_matching_join_within_ttl` |
| 2 | Expired notes are dropped (stale exactly at the TTL boundary) and the join falls back to the honest unknown (`consume` → `None`) | `expired_note_is_dropped_and_join_falls_back_to_unknown` |
| 3 | Notes are keyed by guild+member: a note in one guild never credits another guild or a different member | `notes_are_keyed_by_guild_and_member` |
| 4 | Dying between add and event (note present, no consumer survives) still reports unknown: a fresh tracker attributes nothing, never a fabricated source | `dying_between_add_and_event_reports_unknown_never_fabricated` |
| 5 | `EXPECTED_JOIN_TTL_SECONDS` and `WEB_ONE_CLICK_SOURCE` equal their legacy values (30, `web:one_click`) | `constants_match_legacy_values` |

## Not covered here

- The gateway wiring (`crates/discord/src/pipeline.rs`: `expect_join` staging
  before the add call, `consume` on `MemberAdd` beating the invite diff) is
  outside this card; this suite proves the note semantics the wiring relies on.
- Invite-diff attribution itself (`unknown`/`vanity` fallback) is unchanged
  and covered by the existing tracker tests.
