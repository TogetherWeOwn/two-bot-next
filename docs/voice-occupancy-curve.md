# Voice occupancy-to-join curve (seeding TEST)

Status: measuring. No study or case data exists on voice-seeding rewards or
occupancy-to-join curves; adjacent critical-mass theory only
(`https://digitalcommons.njit.edu/fac_pubs/6343/`). This instrumentation is
the measure-first step; the voice-seeding rewards card builds on its output.

## What is recorded

Every organic ticketed `VOICE_STATE_UPDATE` join increments, at gateway
receipt:

- Metric: `two_bot_voice_join_occupancy_total{occupancy="0|1|..|6|7+"}`.
  `occupancy` is the humans already present in the target channel, joiner
  excluded, capped at `7+`. Fixed 8-series vocabulary; no channel, member,
  guild or time labels ever reach exposition.

No per-event log line is emitted: `VOICE_STATE_UPDATE` is hot per
`docs/log-volume-guard.md`. Hourly curves come from
`increase(two_bot_voice_join_occupancy_total[1h])`.

Leaves, same-channel updates and pre-`READY` frames emit no sample.
Mechanical transitions emit no sample either: joining a creator or
generator channel, and the bot's own move echo out of one into the new
room. Both keep their ticket so room creation is unaffected. Joins into
the session lobby or permanent protected channels (lounges) still sample:
those are places people choose to join, not room-creation steps. Bot joins
(music bots) are ticketed samples too; filter them downstream if they
pollute the curve.

## Join share by occupancy

Overall distribution (Prometheus):

```promql
sum by (occupancy) (increase(two_bot_voice_join_occupancy_total[7d]))
```

Divide each bucket by the total for the empirical join share at that
occupancy: P(occupancy | join). Per-hour curves run the same query with
`increase(...[1h])`. Each process serves one configured guild, so the
counter is per-guild by process. Per-channel curves need channel-scoped
logging, which the ID-free contract deliberately withholds; the rewards
card decides whether that extra scope is worth its cardinality cost.
P(join | occupancy) needs an exposure denominator (for example,
channel-seconds spent at each occupancy), which nothing records yet.

## Worked example

Seven days, one guild, 1,000 sampled joins:

| occupancy | joins | share |
| --- | --- | --- |
| 0 | 80 | 0.08 |
| 1 | 210 | 0.21 |
| 2 | 260 | 0.26 |
| 3 | 190 | 0.19 |
| 4 | 120 | 0.12 |
| 5 | 70 | 0.07 |
| 6 | 40 | 0.04 |
| 7+ | 30 | 0.03 |

Read: joins concentrate at occupancy 1-3. That is a join *share*,
not a conversion rate and not a causal lift: it does not say empty
channels convert worst, nor that seeding a channel to 2 occupants
causes 3x the joins of an empty one. The rewards card must control for
time-of-day, channel kind and guild size before paying for seeds, and
needs an exposure denominator before claiming P(join | occupancy).

## Caveats

- Samples are ticketed joins only: the bot must be live and the guild
  snapshotted. Downtime gaps are missing samples, not zero joins.
- Mechanical transitions are excluded by design: creator and generator joins
  and the bot's move echo keep their ticket but emit no sample, so the 0
  bucket counts organic joins into empty channels, not room creations.
  Session-lobby and permanent protected-channel joins are not mechanical
  and still sample.
- Occupancy counts humans (`counts_as_human`); unknown identities count
  as human, known bots do not, but bot *joiners* still sample.
- `7+` hides the tail; if it exceeds ~10% of samples, raise the cap.
- Join share is not conversion: without channel-time per occupancy bucket,
  do not read the table as P(join | occupancy).
