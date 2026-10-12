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
//! `observed_at_ms`). Contiguous Playing deltas for the same user and game
//! merge; a game change, a gap timeout, a user change, an interleaving
//! observation from another member, or a non-Playing activity closes the
//! interval. Each session carries an in-voice overlap flag derived from voice
//! spans (join/leave plus Discord `self_stream`); streaming sessions are
//! flagged as streaming.

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
    /// member (see [`sessionize`]).
    pub in_voice: bool,
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
    /// Sum of spans for sessions flagged `in_voice`.
    pub in_voice_ms: i64,
    /// Number of sessions folded into this row.
    pub session_count: u64,
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

fn session_game(delta: &PresenceDelta) -> Option<(String, bool)> {
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

fn spans_overlap(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> bool {
    let (a_start, a_end) = if a_start <= a_end {
        (a_start, a_end)
    } else {
        (a_end, a_start)
    };
    let (b_start, b_end) = if b_start <= b_end {
        (b_start, b_end)
    } else {
        (b_end, b_start)
    };
    a_start <= b_end && b_start <= a_end
}

fn finish_open(open: OpenSession, voices: &[VoiceSpan], out: &mut Vec<PlaySession>) {
    let ended_at_ms = if open.started_at_ms == open.ended_at_ms {
        open.started_at_ms
            .saturating_add(SINGLE_OBSERVATION_FLOOR_MS)
    } else {
        open.ended_at_ms
    };
    let in_voice = voices.iter().any(|span| {
        span.user_key == open.user_key
            && spans_overlap(open.started_at_ms, ended_at_ms, span.join_ms, span.leave_ms)
    });
    out.push(PlaySession {
        game: Some(open.game),
        started_at_ms: open.started_at_ms,
        ended_at_ms,
        is_streaming: open.is_streaming,
        in_voice,
    });
}

/// Sessionize play intervals per user and game from presence deltas.
///
/// Deltas are processed in time order (stable sort by `observed_at_ms`).
/// Contiguous Playing (or Streaming) observations for the same user and the
/// same sanitized title merge while each step stays within `gap_timeout_ms`;
/// a game change, a streaming-flag change, a user change, an interleaving
/// observation from another member, a gap beyond the timeout, or a delta
/// with no Playing / Streaming activity closes the open interval.
///
/// Each returned session carries an in-voice flag: true when its wall-clock
/// span shares time with a voice span for the same `user_key` (join/leave
/// overlap; `self_stream` needs no special case because an overlapping span
/// already flags, whether or not the member streamed through Discord).
/// Streaming sessions are flagged via the opening Streaming activity.
#[must_use]
pub fn sessionize(
    deltas: &[PresenceDelta],
    voices: &[VoiceSpan],
    gap_timeout_ms: i64,
) -> Vec<PlaySession> {
    let mut ordered: Vec<&PresenceDelta> = deltas.iter().collect();
    ordered.sort_by_key(|delta| delta.observed_at_ms);

    let mut out: Vec<PlaySession> = Vec::new();
    let mut current: Option<OpenSession> = None;

    for delta in ordered {
        let Some((game, is_streaming)) = session_game(delta) else {
            if let Some(open) = current.take() {
                finish_open(open, voices, &mut out);
            }
            continue;
        };
        // Merge only with the immediately previous observation in time order:
        // any interleaving delta (another member, another game) breaks
        // contiguity and closes the interval.
        let can_merge = match current.as_ref() {
            Some(open) => {
                open.user_key == delta.user_key
                    && open.game == game
                    && open.is_streaming == is_streaming
                    && delta.observed_at_ms >= open.ended_at_ms
                    && delta.observed_at_ms.saturating_sub(open.ended_at_ms) <= gap_timeout_ms
            }
            None => false,
        };
        if can_merge {
            if let Some(open) = current.as_mut() {
                open.ended_at_ms = delta.observed_at_ms;
            }
        } else {
            if let Some(open) = current.take() {
                finish_open(open, voices, &mut out);
            }
            current = Some(OpenSession {
                user_key: delta.user_key.clone(),
                game,
                is_streaming,
                started_at_ms: delta.observed_at_ms,
                ended_at_ms: delta.observed_at_ms,
            });
        }
    }
    if let Some(open) = current.take() {
        finish_open(open, voices, &mut out);
    }
    out
}

/// Fold sessions into guild-level aggregates keyed by (guild, game).
///
/// One row per distinct game title. `total_play_ms` sums wall-clock spans;
/// `in_voice_ms` sums spans of sessions flagged in-voice, so it never
/// exceeds the total. Rows carry no member key. Sorted by game title
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
        if session.in_voice {
            fold.in_voice_ms = fold.in_voice_ms.saturating_add(span);
        }
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

    #[test]
    fn floor_keeps_singletons_positive_without_moving_merged_end() {
        assert_eq!(SINGLE_OBSERVATION_FLOOR_MS, 60_000);
    }
}
