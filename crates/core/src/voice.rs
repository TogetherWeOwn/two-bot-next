//! Open voice sessions + blind-window reconcile (port of two-bot
//! `src/core/voiceSessions.ts`).
//!
//! In memory on purpose: Discord serves no voice history over REST, so the
//! only witness to a session start is the gateway event already seen. A
//! restart loses every open session; the honest end row is then
//! `start_known: false` with a null duration, never a duration measured from
//! boot. The adaptor clears the tracker on both `Resumed` and `Ready`
//! (TOG-6123).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{funnel::parse_iso_millis, Snowflake};

/// One open voice session: the channel the END will be credited to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    pub channel_id: Snowflake,
    pub started_at: String,
    pub session_key: Option<String>,
}

/// Who is in voice right now. One entry per member per guild: Discord allows
/// a member exactly one voice channel, so a move is end(A) then start(B).
#[derive(Debug, Default)]
pub struct VoiceSessionTracker {
    open: HashMap<(Snowflake, Snowflake), OpenSession>,
}

impl VoiceSessionTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a member is now in voice. Replaces any session already open.
    pub fn start(
        &mut self,
        guild_id: Snowflake,
        member_id: Snowflake,
        channel_id: Snowflake,
        started_at: String,
        session_key: Option<String>,
    ) {
        self.open.insert(
            (guild_id, member_id),
            OpenSession {
                channel_id,
                started_at,
                session_key,
            },
        );
    }

    /// Close the open session and return what was known. `None` means the
    /// start was never seen (bot came up mid-session).
    pub fn end(&mut self, guild_id: Snowflake, member_id: Snowflake) -> Option<OpenSession> {
        self.open.remove(&(guild_id, member_id))
    }

    /// Look without closing (TOG-6122: server-leave closes via `on_voice_leave`).
    #[must_use]
    pub fn peek(&self, guild_id: Snowflake, member_id: Snowflake) -> Option<&OpenSession> {
        self.open.get(&(guild_id, member_id))
    }

    /// Drop everything: gateway reconnected and open state is unproven.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// How many sessions are held open. Diagnostics and tests.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

/// Resolved end-of-session measurement (port of `FunnelHandlers.onVoiceLeave`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEnd {
    /// Channel credited: the session's channel, falling back to the
    /// leave frame's channel when the start was never seen.
    pub channel_id: Snowflake,
    /// Stamped time: the leave time, or processing time when the leave
    /// timestamp was unparseable (TOG-7512 — never store garbage in the row).
    pub end_at: String,
    pub start_known: bool,
    pub started_at: Option<String>,
    /// Whole seconds, clamped at zero; `None` when unmeasurable.
    pub duration_seconds: Option<i64>,
}

/// Resolve a voice leave against the tracker. `at` is the leave frame's
/// timestamp; `now` is processing time for the TOG-7512 fallback.
#[must_use]
pub fn resolve_voice_end(
    open: Option<OpenSession>,
    leave_channel_id: Snowflake,
    at: &str,
    now: &str,
) -> VoiceEnd {
    let leave_ms = parse_iso_millis(at);
    let start_ms = open.as_ref().and_then(|o| parse_iso_millis(&o.started_at));
    let start_known = open.is_some() && leave_ms.is_some() && start_ms.is_some();
    let (started_at, duration_seconds) = if start_known {
        let (leave, start) = (leave_ms.unwrap_or(0), start_ms.unwrap_or(0));
        (
            open.as_ref().map(|o| o.started_at.clone()),
            Some((leave - start).div_euclid(1000).max(0)),
        )
    } else {
        (None, None)
    };
    VoiceEnd {
        channel_id: open.as_ref().map_or(leave_channel_id, |o| o.channel_id),
        end_at: if leave_ms.is_some() {
            at.to_owned()
        } else {
            now.to_owned()
        },
        start_known,
        started_at,
        duration_seconds,
    }
}

// --- blind-window reconcile (TOG-5683) --------------------------------------
//
// A voice gap while the bot is down can never be recovered, but it can be
// quantified: gaps in the bot's own write series show when it stopped
// looking, and every `voice_session_end` with `start_known: false` after a
// gap is a session that gap made unmeasurable. Counts, never averages.

/// One interval in which the bot was not looking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlindWindow {
    pub start: String,
    pub end: String,
    pub gap_ms: i64,
}

/// A blind window plus the unknown-start ends attributed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlindWindowCount {
    pub window: BlindWindow,
    /// `voice_session_end` rows with `start_known: false`. A count, never a mean.
    pub unknown_starts: usize,
}

/// Write-series cadence breach declaring a blind window (default suits an
/// approximately hourly series: twice the cadence tolerates one missed tick).
pub const DEFAULT_BLIND_WINDOW_MAX_GAP_MS: i64 = 2 * 60 * 60 * 1000;

/// Every gap between consecutive heartbeats wider than `max_gap_ms`,
/// oldest first. Dedupes, skips unparseable stamps, sorts.
#[must_use]
pub fn find_blind_windows(heartbeats: &[&str], max_gap_ms: i64) -> Vec<BlindWindow> {
    let mut times: Vec<(i64, &str)> = {
        let mut seen = std::collections::HashSet::new();
        heartbeats
            .iter()
            .filter(|s| seen.insert((*s).to_owned()))
            .filter_map(|s| parse_iso_millis(s).map(|t| (t, *s)))
            .collect()
    };
    times.sort();
    times
        .windows(2)
        .filter_map(|w| {
            let gap = w[1].0 - w[0].0;
            (gap > max_gap_ms).then(|| BlindWindow {
                start: w[0].1.to_owned(),
                end: w[1].1.to_owned(),
                gap_ms: gap,
            })
        })
        .collect()
}

/// Attribute each `start_known: false` end to the latest window that started
/// at or before it. Known-start ends, malformed stamps, and ends older than
/// every window are skipped (left unattributed, never guessed).
#[must_use]
pub fn count_unknown_starts_per_window(
    windows: &[BlindWindow],
    ends: &[(String, bool)],
) -> Vec<BlindWindowCount> {
    let starts: Vec<Option<i64>> = windows.iter().map(|w| parse_iso_millis(&w.start)).collect();
    let mut out: Vec<BlindWindowCount> = windows
        .iter()
        .cloned()
        .map(|window| BlindWindowCount {
            window,
            unknown_starts: 0,
        })
        .collect();
    for (at, start_known) in ends {
        if *start_known {
            continue;
        }
        let Some(t) = parse_iso_millis(at) else {
            continue;
        };
        let mut best: Option<usize> = None;
        for (i, s) in starts.iter().enumerate() {
            match s {
                Some(st) if *st <= t => best = Some(i),
                _ => break,
            }
        }
        if let Some(i) = best {
            out[i].unknown_starts += 1;
        }
    }
    out
}

// --- duration averages (TOG-5684) -------------------------------------------
//
// The single enforcement point: every average filters on the FLAG
// (`start_known`), never on null durations. An unknown start with a number
// is still excluded — the start was never seen.

/// The only two fields any duration average may read from an end row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceDurationRow {
    pub start_known: bool,
    pub duration_seconds: Option<f64>,
}

/// Parse one `voice_session_end` metadata blob. Unparseable metadata defaults
/// to `start_known: true` with no duration: never claim the start is unknown
/// when the row itself is unreadable.
#[must_use]
pub fn parse_voice_end_metadata(metadata: Option<&serde_json::Value>) -> VoiceDurationRow {
    let Some(m) = metadata.and_then(serde_json::Value::as_object) else {
        return VoiceDurationRow {
            start_known: true,
            duration_seconds: None,
        };
    };
    let start_known = m.get("startKnown").and_then(serde_json::Value::as_bool) != Some(false);
    let duration_seconds = match m.get("durationSeconds") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(_) => None,
    };
    VoiceDurationRow {
        start_known,
        duration_seconds,
    }
}

/// Durations that may enter a mean: known-start ends with a finite,
/// non-negative duration. Everything else is dropped, never zero-filled.
#[must_use]
pub fn known_voice_durations(rows: &[VoiceDurationRow]) -> Vec<f64> {
    rows.iter()
        .filter(|r| r.start_known)
        .filter_map(|r| r.duration_seconds)
        .filter(|d| d.is_finite() && *d >= 0.0)
        .collect()
}

/// Mean over known-start sessions only; `None` when none measured.
#[must_use]
pub fn average_known_voice_duration(rows: &[VoiceDurationRow]) -> Option<f64> {
    let known = known_voice_durations(rows);
    if known.is_empty() {
        return None;
    }
    Some(known.iter().sum::<f64>() / known.len() as f64)
}

/// Mean plus the two counts proving it honest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceDurationSummary {
    pub average_seconds: Option<f64>,
    /// Known-start sessions with a usable duration that entered the mean.
    pub measured: usize,
    /// `start_known: false` ends excluded before averaging.
    pub excluded_unknown_starts: usize,
}

#[must_use]
pub fn summarize_voice_durations(rows: &[VoiceDurationRow]) -> VoiceDurationSummary {
    VoiceDurationSummary {
        average_seconds: average_known_voice_duration(rows),
        measured: known_voice_durations(rows).len(),
        excluded_unknown_starts: rows.iter().filter(|r| !r.start_known).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_replace_and_close() {
        let mut t = VoiceSessionTracker::new();
        t.start(1, 2, 10, "2026-09-20T12:00:00.000Z".to_owned(), None);
        t.start(1, 2, 11, "2026-09-20T12:01:00.000Z".to_owned(), None);
        assert_eq!(t.open_count(), 1);
        let open = t.end(1, 2).expect("open");
        assert_eq!(open.channel_id, 11);
        assert!(t.end(1, 2).is_none());
    }

    #[test]
    fn resolve_measured_and_unknown() {
        let open = OpenSession {
            channel_id: 10,
            started_at: "2026-09-20T12:00:00.000Z".to_owned(),
            session_key: None,
        };
        let end = resolve_voice_end(
            Some(open),
            99,
            "2026-09-20T12:05:30.000Z",
            "2026-09-20T12:05:31.000Z",
        );
        assert_eq!(
            end,
            VoiceEnd {
                channel_id: 10,
                end_at: "2026-09-20T12:05:30.000Z".to_owned(),
                start_known: true,
                started_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
                duration_seconds: Some(330),
            }
        );
        // No open session: credited to the leave channel, unmeasured.
        let end = resolve_voice_end(None, 99, "2026-09-20T12:05:30.000Z", "now");
        assert!(!end.start_known && end.duration_seconds.is_none() && end.channel_id == 99);
        // Garbage leave stamps processing time (TOG-7512), never the garbage.
        let end = resolve_voice_end(None, 99, "garbage", "2026-09-20T12:05:31.000Z");
        assert_eq!(end.end_at, "2026-09-20T12:05:31.000Z");
        assert!(!end.start_known);
        // Negative clamp: clocks disagree, zero not negative.
        let open = OpenSession {
            channel_id: 10,
            started_at: "2026-09-20T12:06:00.000Z".to_owned(),
            session_key: None,
        };
        let end = resolve_voice_end(Some(open), 10, "2026-09-20T12:05:00.000Z", "now");
        assert_eq!(end.duration_seconds, Some(0));
    }

    #[test]
    fn blind_windows_and_counts() {
        let hb = [
            "2026-09-20T10:00:00.000Z",
            "2026-09-20T11:00:00.000Z",
            "2026-09-20T14:30:00.000Z",
        ];
        let w = find_blind_windows(&hb, DEFAULT_BLIND_WINDOW_MAX_GAP_MS);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].gap_ms, 3 * 3600 * 1000 + 30 * 60 * 1000);
        let ends = vec![
            ("2026-09-20T15:00:00.000Z".to_owned(), false),
            ("2026-09-20T15:01:00.000Z".to_owned(), true),
            ("2026-09-19T09:00:00.000Z".to_owned(), false),
        ];
        let c = count_unknown_starts_per_window(&w, &ends);
        assert_eq!(c[0].unknown_starts, 1);
    }

    #[test]
    fn averages_filter_on_flag_not_null() {
        let rows = vec![
            VoiceDurationRow {
                start_known: true,
                duration_seconds: Some(60.0),
            },
            VoiceDurationRow {
                start_known: false,
                duration_seconds: Some(60.0),
            },
            VoiceDurationRow {
                start_known: true,
                duration_seconds: None,
            },
            VoiceDurationRow {
                start_known: true,
                duration_seconds: Some(-5.0),
            },
        ];
        assert_eq!(average_known_voice_duration(&rows), Some(60.0));
        let s = summarize_voice_durations(&rows);
        assert_eq!(s.measured, 1);
        assert_eq!(s.excluded_unknown_starts, 1);
        // Unparseable metadata never claims unknown start.
        let r = parse_voice_end_metadata(None);
        assert!(r.start_known && r.duration_seconds.is_none());
    }
}
