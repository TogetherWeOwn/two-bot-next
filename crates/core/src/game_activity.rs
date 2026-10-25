//! Game-activity analytics pipeline: play time, in-voice overlap, streaming.
//!
//! Pure and offline: fixtures only, no Discord client, no SQL, no network.
//! The only durable output is guild-level aggregates keyed by (guild, game):
//! total play ms, in-voice ms, session counts. Never a per-member row, never
//! a per-member reader, never a public-surface view. This follows the
//! presence-probe containment precedent (`crate::presence`): guild aggregates,
//! no gateway presence intent, no public reader.
//!
//! Inputs are presence deltas carrying `activities[]` with name plus numeric
//! type (Playing / Streaming / Listening / Watching / Custom / Competing),
//! timestamps, `application_id`, details / state / party / assets. Buttons are
//! label-only (no URL survives). Custom status never creates a session
//! (withheld in large guilds). Streaming URLs survive only for Twitch and
//! YouTube.
//!
//! Sessionization processes deltas in time order (stable sort by
//! `observed_at_ms`) tracking one open interval per member. Contiguous
//! Playing deltas for the same user and game merge; a game change, a gap
//! timeout, or a non-Playing activity from that same member closes their
//! interval. Other members' observations never disturb it. Each session
//! carries its in-voice overlap in millis, summed from voice spans
//! (join/leave plus Discord `self_stream`); streaming sessions are flagged
//! as streaming.

use serde::{Deserialize, Serialize};

/// Longest activity title kept, in characters.
///
/// Matches the voice-presence bound so room names and analytics agree on
/// what a game title looks like.
pub const MAX_GAME_TEXT_CHARS: usize = 128;

/// Floor credited to a session built from a single observation, in millis.
///
/// One sighting proves the member was playing at that instant but says nothing
/// about duration. Multi-observation intervals use wall-clock span
/// (`last - first`); singletons would otherwise be zero-length and vanish
/// from totals, so they count one minute (one polling tick, conservative
/// against inflation).
pub const SINGLE_OBSERVATION_FLOOR_MS: i64 = 60_000;

/// Discord activity kinds the pipeline reads (the gateway `type` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityType {
    /// `0`: playing a game.
    Playing,
    /// `1`: live on an external platform (Twitch, YouTube).
    Streaming,
    /// `2`: listening.
    Listening,
    /// `3`: watching.
    Watching,
    /// `4`: custom status. Never creates a session.
    Custom,
    /// `5`: competing.
    Competing,
}

/// One wire activity, reduced to what analytics keeps.
///
/// Buttons are label-only: `button_urls` never survives [`sanitize_activity`].
/// `stream_url` survives only for Twitch / YouTube
/// (see [`is_allowed_stream_url`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivitySnapshot {
    /// Numeric activity type.
    pub activity_type: ActivityType,
    /// Activity or game title.
    pub name: String,
    /// Discord application id, when present.
    pub application_id: Option<String>,
    /// Rich-presence details line, when present.
    pub details: Option<String>,
    /// Rich-presence state line, when present.
    pub state: Option<String>,
    /// `party.size` as `[current, max]`.
    pub party_size: Option<[u32; 2]>,
    /// Whether rich-presence assets were attached.
    pub assets: bool,
    /// Button labels. Labels survive; URLs never do.
    pub button_labels: Vec<String>,
    /// Button URLs. Always emptied by [`sanitize_activity`].
    pub button_urls: Vec<String>,
    /// Stream URL. Kept only when [`is_allowed_stream_url`] holds.
    pub stream_url: Option<String>,
}

/// One presence observation for one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceDelta {
    /// Opaque member key. Used only to match voice spans during
    /// `sessionize`; never leaves this module in any durable output.
    pub user_key: String,
    /// Epoch millis of the observation.
    pub observed_at_ms: i64,
    /// Activities reported on this observation.
    pub activities: Vec<ActivitySnapshot>,
}

/// One voice session span for one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSpan {
    /// Opaque member key, matched against the delta `user_key`.
    pub user_key: String,
    /// Epoch millis of the join.
    pub join_ms: i64,
    /// Epoch millis of the leave.
    pub leave_ms: i64,
    /// Discord voice-state `self_stream`.
    pub self_stream: bool,
}

/// One sessionized play or streaming interval.
///
/// Ephemeral: built by [`sessionize`], folded by [`aggregate_guild`], never
/// persisted per member. Carries no member key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaySession {
    /// Sanitized game or stream title.
    pub game: Option<String>,
    /// Epoch millis of the first observation in the interval.
    pub started_at_ms: i64,
    /// Epoch millis of the last observation, or `started + floor` for
    /// single-observation intervals (see [`SINGLE_OBSERVATION_FLOOR_MS`]).
    pub ended_at_ms: i64,
    /// True when the opening activity was Streaming.
    pub is_streaming: bool,
    /// True when the interval shares time with a voice span for the same
    /// member (see [`sessionize`]). Equivalent to `in_voice_ms > 0`.
    pub in_voice: bool,
    /// Millis of the interval spent in voice: the summed intersection of
    /// the session span with that member's voice spans, merged so
    /// overlapping spans are never double-counted.
    pub in_voice_ms: i64,
}

/// Guild-level aggregate for one (guild, game).
///
/// The pipeline's only durable output. Carries no member key in any field,
/// `Debug`, or serialized form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildGameAggregate {
    /// Guild the sessions were observed in.
    pub guild_id: String,
    /// Game title, when the sessions carried one.
    pub game: Option<String>,
    /// Sum of session wall-clock spans in millis.
    pub total_play_ms: i64,
    /// Sum of session voice-overlap millis (see [`PlaySession::in_voice_ms`]).
    pub in_voice_ms: i64,
    /// Number of sessions folded into this row.
    pub session_count: u64,
}

/// Most per-game rows the operator report renders for one guild.
pub const GAME_ACTIVITY_REPORT_MAX_ROWS: usize = 100;

/// Rendering of an untitled game (`game: None`, stored as `''`).
pub const GAME_ACTIVITY_UNTITLED_PLACEHOLDER: &str = "(untitled)";

/// Unscoped invocation (no `--guild`): the report object shape with a null
/// guild, no rows and a hint naming the flag, so the CLI exits 0 without
/// opening a database (the `render_reengagement_unscoped` precedent).
#[must_use]
pub fn render_game_activity_unscoped() -> String {
    let mut report = serde_json::json!({
        "guild_id": serde_json::Value::Null,
        "rows": [],
        "truncated": false,
        "hint": "no --guild given; pass --guild <ID> to report one guild",
    })
    .to_string();
    report.push('\n');
    report
}

/// Read-only aggregate operator report for one guild, as one JSON object:
/// `{"guild_id", "rows", "truncated"}` plus a `"hint"` exactly when no row
/// renders. Each row is exactly `{"game", "total_play_ms", "in_voice_ms",
/// "session_count"}`; the aggregate carries no per-person key and the report
/// must not invent one.
///
/// Rows for other guilds never render (defense in depth behind the
/// guild-scoped store read). Ordering is deterministic and input-order
/// independent: `total_play_ms` desc, `in_voice_ms` desc, then game title
/// asc with the untitled row first among full ties. At most
/// [`GAME_ACTIVITY_REPORT_MAX_ROWS`] rows render, with `"truncated": true`
/// when bounded input was cut.
#[must_use]
pub fn render_game_activity_report(guild_id: &str, rows: &[GuildGameAggregate]) -> String {
    let mut report =
        game_activity_report_value(guild_id, rows, GAME_ACTIVITY_REPORT_MAX_ROWS).to_string();
    report.push('\n');
    report
}

fn game_activity_report_value(
    guild_id: &str,
    rows: &[GuildGameAggregate],
    max_rows: usize,
) -> serde_json::Value {
    let mut scoped: Vec<&GuildGameAggregate> =
        rows.iter().filter(|row| row.guild_id == guild_id).collect();
    scoped.sort_by(|a, b| {
        b.total_play_ms
            .cmp(&a.total_play_ms)
            .then_with(|| b.in_voice_ms.cmp(&a.in_voice_ms))
            // `Option<&str>` order is `None` before any `Some`, then
            // lexicographic: untitled first among full ties, titles asc.
            .then_with(|| a.game.as_deref().cmp(&b.game.as_deref()))
            .then_with(|| b.session_count.cmp(&a.session_count))
    });
    let truncated = scoped.len() > max_rows;
    scoped.truncate(max_rows);
    let rendered: Vec<serde_json::Value> = scoped
        .iter()
        .map(|row| {
            serde_json::json!({
                "game": row.game.as_deref().unwrap_or(GAME_ACTIVITY_UNTITLED_PLACEHOLDER),
                "total_play_ms": row.total_play_ms,
                "in_voice_ms": row.in_voice_ms,
                "session_count": row.session_count,
            })
        })
        .collect();
    let mut value = serde_json::json!({
        "guild_id": guild_id,
        "rows": rendered,
        "truncated": truncated,
    });
    if scoped.is_empty() {
        value["hint"] = serde_json::json!("no game activity recorded for this guild");
    }
    value
}

/// Private Discord view of the same aggregate shape and order as the CLI.
/// Only labels are neutralized; numeric totals are never transformed.
#[must_use]
pub fn render_game_activity_discord_report(guild_id: u64, rows: &[GuildGameAggregate]) -> String {
    let guild_id = guild_id.to_string();
    let total = rows.iter().filter(|row| row.guild_id == guild_id).count();
    let mut report = game_activity_report_value(&guild_id, rows, 10);
    if let Some(rendered) = report["rows"].as_array_mut() {
        for row in rendered {
            if let Some(game) = row["game"].as_str() {
                let single_line: String = game
                    .chars()
                    .map(|ch| match ch {
                        '<' => '＜',
                        '`' => '｀',
                        ch if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') => ' ',
                        ch => ch,
                    })
                    .collect();
                row["game"] =
                    serde_json::json!(crate::message_safety::neutralize_mentions(&single_line));
            }
        }
    }
    let note = if total > 10 {
        format!(
            "\nShowing the top 10 games; {} more games omitted.",
            total - 10
        )
    } else {
        String::new()
    };
    let content = format!("```json\n{report}\n```{note}");
    if crate::message_safety::text_len(&content) <= crate::message_safety::CONTENT_LIMIT {
        return content;
    }
    // Bound labels BEFORE serializing, never clip the finished JSON. Ten
    // 24-unit labels (even all escaped quotes/backslashes), three 64-bit
    // numbers per row, a u64 guild id and both notes fit below 2000 units.
    if let Some(rendered) = report["rows"].as_array_mut() {
        for row in rendered {
            if let Some(game) = row["game"].as_str() {
                if crate::message_safety::text_len(game) > 24 {
                    row["game"] = serde_json::json!(format!(
                        "{}…",
                        crate::message_safety::truncate(game, 23)
                    ));
                }
            }
        }
    }
    report["truncated"] = serde_json::json!(true);
    format!("```json\n{report}\n```{note}\nLong game names were shortened to fit Discord's limit.")
}

fn bounded(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_GAME_TEXT_CHARS).collect())
}

/// Strip a snapshot down to what analytics may keep.
///
/// Keeps labels, drops every button URL, and drops stream URLs outside the
/// Twitch / YouTube allowlist. Never fails; unparseable input maps to `None`
/// fields rather than an error.
#[must_use]
pub fn sanitize_activity(snapshot: &ActivitySnapshot) -> ActivitySnapshot {
    let stream_url = snapshot
        .stream_url
        .as_deref()
        .filter(|url| is_allowed_stream_url(url))
        .map(str::to_owned);
    ActivitySnapshot {
        activity_type: snapshot.activity_type,
        name: snapshot.name.clone(),
        application_id: snapshot.application_id.clone(),
        details: snapshot.details.clone(),
        state: snapshot.state.clone(),
        party_size: snapshot.party_size,
        assets: snapshot.assets,
        button_labels: snapshot.button_labels.clone(),
        button_urls: Vec::new(),
        stream_url,
    }
}

/// Whether a stream URL is kept: Twitch and YouTube only.
///
/// Requires an `http(s)` URL whose host is `twitch.tv`, `youtube.com`, or
/// `youtu.be`, or a subdomain of one. Everything else — including lookalike
/// hosts such as `twitch.evil.example` and non-URL text — is rejected.
#[must_use]
pub fn is_allowed_stream_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == "twitch.tv"
        || host.ends_with(".twitch.tv")
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
}

struct OpenSession {
    user_key: String,
    game: String,
    is_streaming: bool,
    started_at_ms: i64,
    ended_at_ms: i64,
}

/// The game (and streaming flag) one delta asserts, if any.
///
/// Playing wins over Streaming; blank titles assert nothing. The gateway only
/// sends presence on change, so the collector treats the result as the
/// member's current state until the next delta (see its carry map), rather
/// than as one poller tick.
#[must_use]
pub fn session_game(delta: &PresenceDelta) -> Option<(String, bool)> {
    for activity in &delta.activities {
        if activity.activity_type == ActivityType::Playing {
            if let Some(game) = bounded(&activity.name) {
                return Some((game, false));
            }
        }
    }
    for activity in &delta.activities {
        if activity.activity_type == ActivityType::Streaming {
            if let Some(game) = bounded(&activity.name) {
                return Some((game, true));
            }
        }
    }
    None
}

/// Millis of `[started, ended)` spent inside `user_key`'s voice spans.
///
/// Intersections use half-open semantics, so a span that ends exactly when
/// the session starts contributes zero. Same-member spans are merged first
/// so overlapping spans are never double-counted.
fn session_voice_overlap_ms(
    started_at_ms: i64,
    ended_at_ms: i64,
    user_key: &str,
    voices: &[VoiceSpan],
) -> i64 {
    let mut clipped: Vec<(i64, i64)> = voices
        .iter()
        .filter(|span| span.user_key == user_key)
        .map(|span| {
            let (join, leave) = if span.join_ms <= span.leave_ms {
                (span.join_ms, span.leave_ms)
            } else {
                (span.leave_ms, span.join_ms)
            };
            (join.max(started_at_ms), leave.min(ended_at_ms))
        })
        .filter(|(start, end)| start < end)
        .collect();
    if clipped.is_empty() {
        return 0;
    }
    clipped.sort();
    let mut total = 0i64;
    let (mut current_start, mut current_end) = clipped[0];
    for (start, end) in clipped.into_iter().skip(1) {
        if start <= current_end {
            current_end = current_end.max(end);
        } else {
            total = total.saturating_add(current_end.saturating_sub(current_start).max(0));
            current_start = start;
            current_end = end;
        }
    }
    total.saturating_add(current_end.saturating_sub(current_start).max(0))
}

fn finish_open(open: OpenSession, voices: &[VoiceSpan], out: &mut Vec<PlaySession>) {
    let ended_at_ms = if open.started_at_ms == open.ended_at_ms {
        open.started_at_ms
            .saturating_add(SINGLE_OBSERVATION_FLOOR_MS)
    } else {
        open.ended_at_ms
    };
    let in_voice_ms =
        session_voice_overlap_ms(open.started_at_ms, ended_at_ms, &open.user_key, voices);
    out.push(PlaySession {
        game: Some(open.game),
        started_at_ms: open.started_at_ms,
        ended_at_ms,
        is_streaming: open.is_streaming,
        in_voice: in_voice_ms > 0,
        in_voice_ms,
    });
}

/// Sessionize play intervals per user and game from presence deltas.
///
/// Deltas are processed in time order (stable sort by `observed_at_ms`)
/// with one open interval tracked per member. Contiguous Playing (or
/// Streaming) observations for the same user and the same sanitized title
/// merge while each step stays within `gap_timeout_ms`; a game change, a
/// streaming-flag change, a gap beyond the timeout, or a delta with no
/// Playing / Streaming activity from that same member closes their open
/// interval. Other members' observations never disturb it.
///
/// Each returned session carries its in-voice overlap in millis: the summed
/// intersection of the session span with voice spans for the same
/// `user_key` (join/leave overlap, merged so overlapping spans count once;
/// `self_stream` needs no special case because an overlapping span already
/// contributes, whether or not the member streamed through Discord).
/// Streaming sessions are flagged via the opening Streaming activity.
/// Output is sorted by `(started_at_ms, ended_at_ms, game, is_streaming,
/// in_voice_ms)` for determinism: ties on start and end (likely when a
/// poller stamps many members with the same tick) still come out in the
/// same order on every run.
#[must_use]
pub fn sessionize(
    deltas: &[PresenceDelta],
    voices: &[VoiceSpan],
    gap_timeout_ms: i64,
) -> Vec<PlaySession> {
    use std::collections::BTreeMap;

    let mut ordered: Vec<&PresenceDelta> = deltas.iter().collect();
    ordered.sort_by_key(|delta| delta.observed_at_ms);

    let mut out: Vec<PlaySession> = Vec::new();
    let mut open_by_user: BTreeMap<String, OpenSession> = BTreeMap::new();

    for delta in ordered {
        let Some((game, is_streaming)) = session_game(delta) else {
            // A non-playing delta closes only its own member's interval.
            if let Some(open) = open_by_user.remove(&delta.user_key) {
                finish_open(open, voices, &mut out);
            }
            continue;
        };
        // Merge only with this member's own previous observation: another
        // member's interleaving deltas must not break contiguity.
        let can_merge = match open_by_user.get(&delta.user_key) {
            Some(open) => {
                open.game == game
                    && open.is_streaming == is_streaming
                    && delta.observed_at_ms >= open.ended_at_ms
                    && delta.observed_at_ms.saturating_sub(open.ended_at_ms) <= gap_timeout_ms
            }
            None => false,
        };
        if can_merge {
            if let Some(open) = open_by_user.get_mut(&delta.user_key) {
                open.ended_at_ms = delta.observed_at_ms;
            }
        } else {
            if let Some(open) = open_by_user.remove(&delta.user_key) {
                finish_open(open, voices, &mut out);
            }
            open_by_user.insert(
                delta.user_key.clone(),
                OpenSession {
                    user_key: delta.user_key.clone(),
                    game,
                    is_streaming,
                    started_at_ms: delta.observed_at_ms,
                    ended_at_ms: delta.observed_at_ms,
                },
            );
        }
    }
    let mut remaining: Vec<OpenSession> = open_by_user.into_values().collect();
    remaining.sort_by(|a, b| {
        (a.started_at_ms, a.ended_at_ms, &a.game, a.is_streaming).cmp(&(
            b.started_at_ms,
            b.ended_at_ms,
            &b.game,
            b.is_streaming,
        ))
    });
    for open in remaining {
        finish_open(open, voices, &mut out);
    }
    out.sort_by(|a, b| {
        (
            a.started_at_ms,
            a.ended_at_ms,
            &a.game,
            a.is_streaming,
            a.in_voice_ms,
        )
            .cmp(&(
                b.started_at_ms,
                b.ended_at_ms,
                &b.game,
                b.is_streaming,
                b.in_voice_ms,
            ))
    });
    out
}

/// Fold sessions into guild-level aggregates keyed by (guild, game).
///
/// One row per distinct game title. `total_play_ms` sums wall-clock spans;
/// `in_voice_ms` sums each session's stored voice-overlap millis, so it
/// never exceeds the total. Rows carry no member key. Sorted by game title
/// (`None` first) for deterministic output.
#[must_use]
pub fn aggregate_guild(guild_id: &str, sessions: &[PlaySession]) -> Vec<GuildGameAggregate> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fold {
        total_play_ms: i64,
        in_voice_ms: i64,
        session_count: u64,
    }

    let mut by_game: BTreeMap<Option<String>, Fold> = BTreeMap::new();
    for session in sessions {
        let fold = by_game.entry(session.game.clone()).or_default();
        let span = session
            .ended_at_ms
            .saturating_sub(session.started_at_ms)
            .max(0);
        fold.total_play_ms = fold.total_play_ms.saturating_add(span);
        fold.in_voice_ms = fold.in_voice_ms.saturating_add(session.in_voice_ms.max(0));
        fold.session_count = fold.session_count.saturating_add(1);
    }

    by_game
        .into_iter()
        .map(|(game, fold)| GuildGameAggregate {
            guild_id: guild_id.to_owned(),
            game,
            total_play_ms: fold.total_play_ms,
            in_voice_ms: fold.in_voice_ms.min(fold.total_play_ms),
            session_count: fold.session_count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_MS: i64 = 60_000;

    fn playing_snapshot(name: &str) -> ActivitySnapshot {
        ActivitySnapshot {
            activity_type: ActivityType::Playing,
            name: name.to_owned(),
            application_id: None,
            details: None,
            state: None,
            party_size: None,
            assets: false,
            button_labels: Vec::new(),
            button_urls: Vec::new(),
            stream_url: None,
        }
    }

    fn user_delta(user_key: &str, at_ms: i64, game: &str) -> PresenceDelta {
        PresenceDelta {
            user_key: user_key.to_owned(),
            observed_at_ms: at_ms,
            activities: vec![playing_snapshot(game)],
        }
    }

    #[test]
    fn floor_keeps_singletons_positive_without_moving_merged_end() {
        assert_eq!(SINGLE_OBSERVATION_FLOOR_MS, 60_000);
    }

    #[test]
    fn interleaved_members_keep_their_own_sessions() {
        // Two members each play Apex for 20 minutes, updates every 5 minutes
        // offset by 1 minute. Per-user sessionization must yield 2 sessions
        // totalling ~40 minutes, not 10 singletons totalling 10 minutes.
        let t0 = 1_750_000_000_000i64;
        let mut deltas = Vec::new();
        for step in 0..5 {
            deltas.push(user_delta("user-a", t0 + step * 5 * MIN_MS, "Apex"));
            deltas.push(user_delta(
                "user-b",
                t0 + MIN_MS + step * 5 * MIN_MS,
                "Apex",
            ));
        }
        let sessions = sessionize(&deltas, &[], 15 * MIN_MS);
        assert_eq!(
            sessions.len(),
            2,
            "one session per member, got {sessions:?}"
        );
        let total: i64 = sessions
            .iter()
            .map(|s| s.ended_at_ms.saturating_sub(s.started_at_ms).max(0))
            .sum();
        assert_eq!(total, 40 * MIN_MS, "both members' play time kept");
    }

    #[test]
    fn in_voice_ms_counts_overlap_not_whole_span() {
        // 30-minute session sharing 1 minute with a voice span must report
        // 1 minute in-voice, not the whole 30-minute span.
        let t0 = 1_750_000_000_000i64;
        let deltas = vec![
            user_delta("user-1", t0, "Apex"),
            user_delta("user-1", t0 + 30 * MIN_MS, "Apex"),
        ];
        let voices = vec![VoiceSpan {
            user_key: "user-1".to_owned(),
            join_ms: t0,
            leave_ms: t0 + MIN_MS,
            self_stream: false,
        }];
        let sessions = sessionize(&deltas, &voices, 60 * MIN_MS);
        assert_eq!(sessions.len(), 1);
        let rows = aggregate_guild("guild-1", &sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].in_voice_ms, MIN_MS,
            "in-voice time is the 1-minute overlap, got {:?}",
            rows[0]
        );
    }

    #[test]
    fn other_members_non_playing_delta_leaves_session_open() {
        // Member A plays at t0, t0+5m, t0+10m; member B plays at t0+2m then
        // sends an empty-activities delta at t0+3m. B's non-playing delta
        // must close only B, leaving A as one 10-minute session.
        let t0 = 1_750_000_000_000i64;
        let deltas = vec![
            user_delta("user-a", t0, "Apex"),
            user_delta("user-b", t0 + 2 * MIN_MS, "Apex"),
            PresenceDelta {
                user_key: "user-b".to_owned(),
                observed_at_ms: t0 + 3 * MIN_MS,
                activities: Vec::new(),
            },
            user_delta("user-a", t0 + 5 * MIN_MS, "Apex"),
            user_delta("user-a", t0 + 10 * MIN_MS, "Apex"),
        ];
        let sessions = sessionize(&deltas, &[], 15 * MIN_MS);
        assert_eq!(
            sessions.len(),
            2,
            "one session per member, got {sessions:?}"
        );
        let total: i64 = sessions
            .iter()
            .map(|s| s.ended_at_ms.saturating_sub(s.started_at_ms).max(0))
            .sum();
        assert_eq!(
            total,
            11 * MIN_MS,
            "A spans 10 min plus B's 1-minute floor, got {sessions:?}"
        );
        assert!(
            sessions
                .iter()
                .any(|s| s.ended_at_ms.saturating_sub(s.started_at_ms) == 10 * MIN_MS),
            "A stays one 10-minute session, got {sessions:?}"
        );
        assert!(
            sessions
                .iter()
                .any(|s| s.ended_at_ms.saturating_sub(s.started_at_ms) == MIN_MS),
            "B is one floor-credited singleton, got {sessions:?}"
        );
    }

    #[test]
    fn overlapping_voice_spans_merge_and_boundary_is_zero() {
        // Two overlapping spans for one member inside a 30-minute session
        // must count their union once (5..20 = 15 min). A span ending
        // exactly at the session start contributes zero (half-open).
        let t0 = 1_750_000_000_000i64;
        let deltas = vec![
            user_delta("user-1", t0, "Apex"),
            user_delta("user-1", t0 + 30 * MIN_MS, "Apex"),
        ];
        let voices = vec![
            VoiceSpan {
                user_key: "user-1".to_owned(),
                join_ms: t0 + 5 * MIN_MS,
                leave_ms: t0 + 15 * MIN_MS,
                self_stream: false,
            },
            VoiceSpan {
                user_key: "user-1".to_owned(),
                join_ms: t0 + 10 * MIN_MS,
                leave_ms: t0 + 20 * MIN_MS,
                self_stream: false,
            },
            VoiceSpan {
                user_key: "user-1".to_owned(),
                join_ms: t0 - 10 * MIN_MS,
                leave_ms: t0,
                self_stream: false,
            },
        ];
        let sessions = sessionize(&deltas, &voices, 60 * MIN_MS);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].in_voice_ms,
            15 * MIN_MS,
            "union of overlapping spans is 15 min, got {:?}",
            sessions[0]
        );
        assert_eq!(
            session_voice_overlap_ms(t0, t0 + 30 * MIN_MS, "user-1", &voices[2..3]),
            0,
            "span ending exactly at the session start contributes zero"
        );
    }

    #[test]
    fn tied_sessions_have_deterministic_total_order() {
        // Two singletons stamped at the same tick tie on (start, end).
        // The total sort key (game, streaming, voice) must order them the
        // same way on every run.
        let t0 = 1_750_000_000_000i64;
        let deltas = vec![
            user_delta("user-a", t0, "Zelda"),
            user_delta("user-b", t0, "Apex"),
        ];
        let first = sessionize(&deltas, &[], 15 * MIN_MS);
        let second = sessionize(&deltas, &[], 15 * MIN_MS);
        assert_eq!(first.len(), 2);
        assert_eq!(first, second, "tied sessions order every run");
        assert_eq!(
            first[0].game.as_deref(),
            Some("Apex"),
            "ties break by game, got {first:?}"
        );
        assert_eq!(
            first[1].game.as_deref(),
            Some("Zelda"),
            "ties break by game, got {first:?}"
        );
    }
}
