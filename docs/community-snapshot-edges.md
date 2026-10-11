# Community snapshot edges

Acceptance note for `two_bot_core::community_snapshots`
(`crates/core/tests/community_snapshot_edges.rs`, TOG-12704). Tests-only;
no `src` changes. All assertions go through the existing public API only —
no REST, timers, or SQL.

## Refusals carry reasons, partial input still builds

The builders return `Option`; the tick maps each `None` to a skip reason
instead of failing the whole snapshot (see `website_store` tick recipe):

- `match_rank_roles` returns `None` (caller: `RankSkip::RankRoleMissing`)
  for partial ladders, duplicate display names, unladdered guilds, and
  empty role lists instead of guessing. Matching stays case-insensitive
  and whitespace-tolerant; unrelated extra guild roles are ignored.
- `build_counter_reading` returns `None` on empty or blank-id rosters.
  Partial input still builds: bots and raid-window accounts leave the
  denominator while `raid_accounts_excluded` names the exclusion count.
- `window_bounds` returns `None` on malformed days and never yields a
  forward window for inverted ranges; the caller treats `from >= to` as
  ungrounded history (`CounterSkip::RaidHistoryNotGrounded`) and publishes
  nothing. All three static `RAID_ANOMALIES` ground to forward windows.
- `build_community_snapshot` returns `None` on empty/blank rosters and on
  short, empty, misordered, or over-long ladders. Partial input still
  builds: unranked humans keep an explicit `rank_key: None` row,
  exclusions are named in `excluded_member_ids`, holder/highest counts stay
  mutually exclusive, and `ranked_member_count <= human_member_count`.
- A higher rank without every lower rung still builds with `nested: false`;
  the rank tick self-heals it first (`plan_rank_heal`: grant the missing
  lower rungs with an audit reason, bounded at 25 grants per tick and
  fenced below the bot's highest role, then rebuild and re-verify) and
  publishes the healed snapshot with a repair alert naming the members
  and roles. A ladder that is still bad afterwards, or an unhealable one
  (over bound, hierarchy refusal), maps to `RankSkip::RanksNotNested`
  (writes nothing) instead of publishing a broken ladder.
- `JobGate` is single-flight: a second `try_acquire` while a tick holds
  the guard returns `None` (skip, do not queue); the gate releases on
  guard drop.

## Verify

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test community_snapshot_edges
```
