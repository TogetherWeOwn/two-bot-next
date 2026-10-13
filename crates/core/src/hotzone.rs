//! Hot-zone time heatmaps: hour-of-day × weekday activity for joins, voice, games.
//!
//! When-to-run-events input: which (weekday, hour) cells the guild actually
//! uses, so event planning can aim at hot cells instead of guessing. Three
//! signals share one 7 × 24 grid each: member joins, voice presence, and game
//! activity.
//!
//! Seed prior (WhatPulse "When people play PC games",
//! <https://whatpulse.org/research/when-people-play-games>, sample window Jun
//! 2025 – Jun 2026): PC-gamer keyboard+mouse activity inside games peaks
//! around 9 PM (8.51% of a day's gaming) and is quietest around 6 AM (0.91%);
//! weekends run heaviest (Sun 16.58%, Sat 15.87%). This is an external
//! convenience sample, not guild truth: [`whatpulse_prior`] seeds the grid,
//! and [`blend_with_prior`] re-weights it weekly from the guild's own
//! observations. The planned consumer is the event-planning card, via
//! [`top_slots`].
//!
//! Everything here is pure: the caller hands in samples with an explicit UTC
//! offset per instant, this module returns plain data. No clock, no
//! environment, no network, no SQL — so it unit-tests without Discord or
//! Postgres. Weekdays are Sunday = 0, matching
//! [`crate::anchor_event`]'s spec convention. DST correctness belongs to the
//! caller: re-resolve the UTC offset per instant (for example with
//! [`crate::onboarding::zoned_epoch_secs`]'s New York rules) instead of
//! passing one fixed offset across a changeover.

/// Hours per day (grid width).
pub const HOURS_PER_DAY: usize = 24;
/// Days per week (grid height).
pub const DAYS_PER_WEEK: usize = 7;
/// Cells per week (flattened grid length).
pub const CELLS_PER_WEEK: usize = DAYS_PER_WEEK * HOURS_PER_DAY;

/// Peak gaming hour in the seed prior (9 PM), in percent of a day's gaming.
pub const PEAK_HOUR: u32 = 21;
/// Quietest gaming hour in the seed prior (6 AM).
pub const QUIET_HOUR: u32 = 6;

/// Seed source URL (methodology: anonymized keyboard+mouse activity in games).
pub const WHAT_PULSE_SOURCE_URL: &str = "https://whatpulse.org/research/when-people-play-games";
/// Seed sample window.
pub const WHAT_PULSE_SAMPLE_WINDOW: &str = "Jun 2025 - Jun 2026";

/// Seed hourly distribution, index = hour 0–23, in percent (sums to 100.0).
/// 12 AM first, matching the published "Key findings" table.
pub const WHAT_PULSE_HOUR_PCT: [f64; HOURS_PER_DAY] = [
    5.35, 3.81, 2.65, 1.83, 1.31, 1.03, 0.91, 0.96, 1.19, 1.66, 2.26, 2.91, 3.44, 4.02, 4.60, 5.21,
    5.74, 6.16, 6.40, 7.02, 7.94, 8.51, 8.22, 6.87,
];

/// Seed weekday distribution, index = Sunday 0 … Saturday 6, in percent
/// (sums to 100.01 through published rounding).
pub const WHAT_PULSE_WEEKDAY_PCT: [f64; DAYS_PER_WEEK] =
    [16.58, 13.59, 13.37, 13.32, 13.38, 13.90, 15.87];

/// Default pseudo-count weight for [`blend_with_prior`]: 168.0, i.e. one
/// prior pseudo-observation per grid cell on average. The weekly re-weight job
/// passes the same weight each run, so a week with no guild observations
/// keeps the prior and a busy week overwhelms it.
pub const DEFAULT_PRIOR_WEIGHT: f64 = 168.0;

/// Which activity a sample records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotzoneSignal {
    /// Member join (guild member add).
    Join,
    /// Voice presence delta (join/leave/move counts as presence at that cell).
    Voice,
    /// Game activity (presence game field, game-role event, or scheduled
    /// game-night attendance once the event store flows).
    Game,
}

impl HotzoneSignal {
    /// Row index into [`HotzoneCounts::cells`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Join => 0,
            Self::Voice => 1,
            Self::Game => 2,
        }
    }
}

/// One timestamped observation. `utc_offset_secs` is the guild-local offset
/// east of UTC at `epoch_secs` (caller-resolved per instant for DST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotzoneSample {
    pub signal: HotzoneSignal,
    pub epoch_secs: i64,
    pub utc_offset_secs: i64,
}

/// Per-signal 7 (Sunday 0) × 24 grids of raw observation counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HotzoneCounts {
    /// `cells[signal][weekday_sun0][hour]`.
    pub cells: [[[u64; HOURS_PER_DAY]; DAYS_PER_WEEK]; 3],
}

impl HotzoneCounts {
    /// Count at one cell (out-of-range weekday/hour reads 0, never panics).
    #[must_use]
    pub fn get(&self, signal: HotzoneSignal, weekday_sun0: u32, hour: u32) -> u64 {
        let (Some(day), Some(hr)) = (
            usize::try_from(weekday_sun0).ok(),
            usize::try_from(hour).ok(),
        ) else {
            return 0;
        };
        self.cells
            .get(signal.index())
            .and_then(|grid| grid.get(day))
            .and_then(|row| row.get(hr))
            .copied()
            .unwrap_or(0)
    }

    /// Total observations for one signal.
    #[must_use]
    pub fn total(&self, signal: HotzoneSignal) -> u64 {
        self.cells[signal.index()]
            .iter()
            .flat_map(|row| row.iter())
            .sum()
    }

    /// One signal's grid flattened to weekday-major order
    /// (`[weekday * 24 + hour]`), the layout [`blend_with_prior`] and
    /// [`top_slots`] consume.
    #[must_use]
    pub fn as_flat(&self, signal: HotzoneSignal) -> [u64; CELLS_PER_WEEK] {
        let mut out = [0_u64; CELLS_PER_WEEK];
        for (day, row) in self.cells[signal.index()].iter().enumerate() {
            for (hour, count) in row.iter().enumerate() {
                out[day * HOURS_PER_DAY + hour] = *count;
            }
        }
        out
    }
}

/// A ranked hot cell for event planning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HotzoneSlot {
    /// Sunday = 0 … Saturday = 6.
    pub weekday_sun0: u32,
    /// Hour of day 0–23.
    pub hour: u32,
    /// Blended probability mass at this cell (sums to 1.0 over the grid).
    pub score: f64,
}

/// Bin an instant to `(weekday_sun0, hour)` in the offset's wall clock.
/// Pure integer maths (`div_euclid`/`rem_euclid`), so pre-epoch instants bin
/// correctly. 1970-01-01 was a Thursday, hence the +4 Sunday-0 shift.
#[must_use]
pub fn cell_for_epoch_secs(epoch_secs: i64, utc_offset_secs: i64) -> (u32, u32) {
    let local = epoch_secs.saturating_add(utc_offset_secs);
    let days = local.div_euclid(86_400);
    let weekday = (days + 4).rem_euclid(DAYS_PER_WEEK as i64) as u32;
    let hour = local.div_euclid(3_600).rem_euclid(HOURS_PER_DAY as i64) as u32;
    (weekday, hour)
}

/// Bin samples into per-signal grids.
#[must_use]
pub fn rollup(samples: impl IntoIterator<Item = HotzoneSample>) -> HotzoneCounts {
    let mut counts = HotzoneCounts::default();
    for sample in samples {
        let (weekday, hour) = cell_for_epoch_secs(sample.epoch_secs, sample.utc_offset_secs);
        counts.cells[sample.signal.index()][weekday as usize][hour as usize] =
            counts.cells[sample.signal.index()][weekday as usize][hour as usize].saturating_add(1);
    }
    counts
}

/// Seed prior over the 168 cells (weekday-major): the separable product of
/// the weekday and hour seed distributions, normalized to sum to 1.0. The
/// argmax is Sunday 21:00 (weekend × evening peak).
#[must_use]
pub fn whatpulse_prior() -> [f64; CELLS_PER_WEEK] {
    let mut out = [0.0_f64; CELLS_PER_WEEK];
    for (day, day_pct) in WHAT_PULSE_WEEKDAY_PCT.iter().enumerate() {
        for (hour, hour_pct) in WHAT_PULSE_HOUR_PCT.iter().enumerate() {
            out[day * HOURS_PER_DAY + hour] = day_pct / 100.0 * hour_pct / 100.0;
        }
    }
    let total: f64 = out.iter().sum();
    if total > 0.0 {
        for cell in out.iter_mut() {
            *cell /= total;
        }
    }
    out
}

/// Empirical distribution of one signal's counts (sums to 1.0; all zeros
/// when there are no observations).
#[must_use]
pub fn normalize(counts: &[u64; CELLS_PER_WEEK]) -> [f64; CELLS_PER_WEEK] {
    let total: u64 = counts.iter().sum();
    let mut out = [0.0_f64; CELLS_PER_WEEK];
    if total == 0 {
        return out;
    }
    let total = total as f64;
    for (slot, count) in out.iter_mut().zip(counts.iter()) {
        *slot = *count as f64 / total;
    }
    out
}

/// Weekly re-weight: posterior mean of the seed prior (spread over
/// `prior_weight` pseudo-observations) and this week's guild counts.
/// `prior_weight <= 0` or non-finite falls back to the pure empirical
/// distribution, so a misconfigured job degrades to guild-only data instead
/// of `NaN` cells.
#[must_use]
pub fn blend_with_prior(
    prior: &[f64; CELLS_PER_WEEK],
    observed: &[u64; CELLS_PER_WEEK],
    prior_weight: f64,
) -> [f64; CELLS_PER_WEEK] {
    let observed_total: u64 = observed.iter().sum();
    if !prior_weight.is_finite() || prior_weight <= 0.0 {
        return normalize(observed);
    }
    let denom = prior_weight + observed_total as f64;
    let mut out = [0.0_f64; CELLS_PER_WEEK];
    for ((slot, prior_cell), count) in out.iter_mut().zip(prior.iter()).zip(observed.iter()) {
        *slot = (prior_weight * prior_cell + *count as f64) / denom;
    }
    out
}

/// Top-`n` cells by blended score, descending; ties break by weekday then
/// hour so the ranking is deterministic. `n` caps at 168.
#[must_use]
pub fn top_slots(dist: &[f64; CELLS_PER_WEEK], n: usize) -> Vec<HotzoneSlot> {
    let mut slots: Vec<HotzoneSlot> = dist
        .iter()
        .enumerate()
        .map(|(i, score)| HotzoneSlot {
            weekday_sun0: (i / HOURS_PER_DAY) as u32,
            hour: (i % HOURS_PER_DAY) as u32,
            score: *score,
        })
        .collect();
    slots.sort_by(|a, b| {
        match (a.score.is_nan(), b.score.is_nan()) {
            // Both finite: descending score via a total order.
            (false, false) => b.score.total_cmp(&a.score),
            // Exactly one NaN: NaN sorts last.
            (a_nan, b_nan) => a_nan.cmp(&b_nan),
        }
        .then_with(|| (a.weekday_sun0, a.hour).cmp(&(b.weekday_sun0, b.hour)))
    });
    slots.truncate(n.min(CELLS_PER_WEEK));
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-9;

    #[test]
    fn hour_prior_sums_to_one_hundred_with_documented_anchors() {
        let total: f64 = WHAT_PULSE_HOUR_PCT.iter().sum();
        assert!((total - 100.0).abs() < EPSILON, "sums to {total}");
        assert_eq!(WHAT_PULSE_HOUR_PCT[PEAK_HOUR as usize], 8.51);
        assert_eq!(WHAT_PULSE_HOUR_PCT[QUIET_HOUR as usize], 0.91);
        assert_eq!(WHAT_PULSE_HOUR_PCT.len(), HOURS_PER_DAY);
    }

    #[test]
    fn weekday_prior_matches_published_weekend_heavy_split() {
        // Sunday 0 … Saturday 6.
        assert_eq!(WHAT_PULSE_WEEKDAY_PCT[0], 16.58);
        assert_eq!(WHAT_PULSE_WEEKDAY_PCT[6], 15.87);
        let total: f64 = WHAT_PULSE_WEEKDAY_PCT.iter().sum();
        assert!((total - 100.0).abs() < 0.02, "sums to {total}");
    }

    #[test]
    fn seed_prior_is_a_distribution_peaking_sunday_evening() {
        let prior = whatpulse_prior();
        let total: f64 = prior.iter().sum();
        assert!((total - 1.0).abs() < EPSILON, "sums to {total}");
        let argmax = prior
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .expect("non-empty grid");
        // Sunday is weekday 0, so its peak cell is at flattened index PEAK_HOUR.
        assert_eq!(argmax, PEAK_HOUR as usize);
        assert!((prior[argmax] - 0.1658 * 0.0851).abs() < 1e-4);
    }

    #[test]
    fn epoch_bins_to_known_weekday_hour_cells() {
        // 1970-01-01T00:00:00Z was a Thursday.
        assert_eq!(cell_for_epoch_secs(0, 0), (4, 0));
        // Same instant is Wednesday 19:00 at UTC-5.
        assert_eq!(cell_for_epoch_secs(0, -5 * 3_600), (3, 19));
        // One second before the epoch is Wednesday 23:00 UTC.
        assert_eq!(cell_for_epoch_secs(-1, 0), (3, 23));
        // A Sunday evening guild night: 2026-10-11T21:00:00-04:00.
        let sunday_9pm_edt = 1_791_766_800;
        assert_eq!(cell_for_epoch_secs(sunday_9pm_edt, -4 * 3_600), (0, 21));
    }

    #[test]
    fn rollup_separates_signals_and_bins_cells() {
        let counts = rollup([
            HotzoneSample {
                signal: HotzoneSignal::Join,
                epoch_secs: 0,
                utc_offset_secs: 0,
            },
            HotzoneSample {
                signal: HotzoneSignal::Join,
                epoch_secs: 0,
                utc_offset_secs: 0,
            },
            HotzoneSample {
                signal: HotzoneSignal::Voice,
                epoch_secs: 0,
                utc_offset_secs: 0,
            },
            HotzoneSample {
                signal: HotzoneSignal::Game,
                epoch_secs: -1,
                utc_offset_secs: 0,
            },
        ]);
        assert_eq!(counts.get(HotzoneSignal::Join, 4, 0), 2);
        assert_eq!(counts.get(HotzoneSignal::Voice, 4, 0), 1);
        assert_eq!(counts.get(HotzoneSignal::Game, 3, 23), 1);
        assert_eq!(counts.get(HotzoneSignal::Join, 3, 23), 0);
        assert_eq!(counts.total(HotzoneSignal::Join), 2);
        // Out-of-range reads are 0, never a panic.
        assert_eq!(counts.get(HotzoneSignal::Join, 7, 0), 0);
        assert_eq!(counts.get(HotzoneSignal::Join, 0, 24), 0);
    }

    #[test]
    fn blend_without_observations_keeps_the_prior() {
        let prior = whatpulse_prior();
        let blended = blend_with_prior(&prior, &[0_u64; CELLS_PER_WEEK], DEFAULT_PRIOR_WEIGHT);
        for (cell, seed) in blended.iter().zip(prior.iter()) {
            assert!((cell - seed).abs() < EPSILON);
        }
    }

    #[test]
    fn blend_converges_to_guild_data_as_observations_grow() {
        let prior = whatpulse_prior();
        let mut observed = [0_u64; CELLS_PER_WEEK];
        // A Wednesday 6 AM guild whose members never match the seed's
        // Sunday-evening peak: 10 000 observations in one off-peak cell.
        observed[3 * HOURS_PER_DAY + 6] = 10_000;
        let blended = blend_with_prior(&prior, &observed, DEFAULT_PRIOR_WEIGHT);
        let total: f64 = blended.iter().sum();
        assert!((total - 1.0).abs() < 1e-6, "sums to {total}");
        let top = top_slots(&blended, 1);
        assert_eq!((top[0].weekday_sun0, top[0].hour), (3, 6));
    }

    #[test]
    fn blend_with_bad_weight_falls_back_to_empirical() {
        let prior = whatpulse_prior();
        let mut observed = [0_u64; CELLS_PER_WEEK];
        observed[42] = 3;
        for weight in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let blended = blend_with_prior(&prior, &observed, weight);
            assert_eq!(blended[42], 1.0);
            assert_eq!(blended.iter().sum::<f64>(), 1.0);
        }
    }

    #[test]
    fn normalize_empty_counts_are_all_zeros() {
        assert_eq!(normalize(&[0_u64; CELLS_PER_WEEK]), [0.0; CELLS_PER_WEEK]);
    }

    #[test]
    fn top_slots_rank_descending_cap_and_break_ties_deterministically() {
        let mut dist = [0.0_f64; CELLS_PER_WEEK];
        dist[10] = 0.5;
        dist[20] = 0.5;
        dist[30] = 0.1;
        let top = top_slots(&dist, 2);
        assert_eq!(top.len(), 2);
        // Tied cells order by weekday then hour.
        assert_eq!((top[0].weekday_sun0, top[0].hour), (0, 10));
        assert_eq!((top[1].weekday_sun0, top[1].hour), (0, 20));
        assert!(top_slots(&dist, 10_000).len() <= CELLS_PER_WEEK);
        assert!(top_slots(&dist, 0).is_empty());
    }

    #[test]
    fn top_slots_ranks_nan_last_without_panicking() {
        // One NaN cell in an otherwise finite grid: the NaN handling must be
        // a total order (no `sort_by` panic) and the finite maximum still
        // ranks first.
        let mut dist = [0.001_f64; CELLS_PER_WEEK];
        dist[10] = 0.5;
        dist[5] = f64::NAN;
        let top = top_slots(&dist, 5);
        assert_eq!(top.len(), 5);
        assert_eq!((top[0].weekday_sun0, top[0].hour), (0, 10));
        assert!(top[..5].iter().all(|slot| slot.score.is_finite()));
        let full = top_slots(&dist, CELLS_PER_WEEK);
        assert_eq!(full.len(), CELLS_PER_WEEK);
        let last = full.last().expect("non-empty grid");
        assert!(last.score.is_nan());
        assert_eq!((last.weekday_sun0, last.hour), (0, 5));
    }

    #[test]
    fn flat_layout_is_weekday_major() {
        let mut counts = HotzoneCounts::default();
        counts.cells[HotzoneSignal::Voice.index()][6][23] = 7;
        let flat = counts.as_flat(HotzoneSignal::Voice);
        assert_eq!(flat[6 * HOURS_PER_DAY + 23], 7);
        assert_eq!(flat.iter().sum::<u64>(), 7);
        assert_eq!(counts.as_flat(HotzoneSignal::Join).iter().sum::<u64>(), 0);
    }
}
