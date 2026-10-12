# Voice occupancy-to-join curve (seeding TEST)

Status: measuring. No study or case data exists on voice-seeding rewards or
occupancy-to-join curves; adjacent critical-mass theory only
(`https://digitalcommons.njit.edu/fac_pubs/6343/`). This instrumentation is
the measure-first step; the voice-seeding rewards card builds on its output.

## What is recorded

Every ticketed `VOICE_STATE_UPDATE` join emits, at gateway receipt:

- Metric: `two_bot_voice_join_occupancy_total{occupancy="0|1|..|6|7+"}`.
  `occupancy` is the humans already present in the target channel, joiner
  excluded, capped at `7+`. Fixed 8-series vocabulary; no channel, member,
  guild or time labels ever reach exposition.
- Log: `voice_event="voice_join_occupancy"` with numeric `occupancy_at_join`.
  Token-free and ID-free, per the observability catalog. Log timestamps give
  the hour; the dispatch span gives guild correlation.

Leaves, same-channel updates and pre-`READY` frames emit no sample. Bot
joins (music bots) are ticketed samples too; filter them downstream if they
pollute the curve.

## Computing P(join | occupancy)

Overall distribution (Prometheus):

```promql
sum by (occupancy) (increase(two_bot_voice_join_occupancy_total[7d]))
```

Divide each bucket by the total for the empirical join share at that
occupancy. Per-hour curves: run the same query per hour, or bucket the
`voice_join_occupancy` log lines by timestamp hour and count
`occupancy_at_join` per hour. Per-guild curves: split log lines by the
dispatch span guild correlation. Per-channel curves need channel-scoped
logging, which the ID-free contract deliberately withholds; the rewards
card decides whether that extra scope is worth its cardinality cost.

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

Read: joins concentrate at occupancy 1-3; empty channels convert worst.
That is a join *share*, not a causal lift: it does not say seeding a
channel to 2 occupants causes 3x the joins of an empty one. The rewards
card must control for time-of-day, channel kind and guild size before
paying for seeds.

## Caveats

- Samples are ticketed joins only: the bot must be live and the guild
  snapshotted. Downtime gaps are missing samples, not zero joins.
- Occupancy counts humans (`counts_as_human`); unknown identities count
  as human, known bots do not, but bot *joiners* still sample.
- `7+` hides the tail; if it exceeds ~10% of samples, raise the cap.
