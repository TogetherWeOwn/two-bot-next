//! Funnel event vocabulary and row identity (port of two-bot `src/core/events.ts`).
//!
//! Every string literal here matches the legacy TypeScript source exactly:
//! event types, source prefixes (`invite:`, `ambiguous:`, `channel:`, `job:`,
//! `backfill:`, `web:one_click`), and the idempotency-key formats. The
//! replay test (`crates/core/tests/funnel_replay.rs`) pins this parity
//! against rows produced by the real legacy handlers.

use serde::{Deserialize, Serialize};

/// Discord snowflake IDs. Legacy stores them as text; the core keeps raw u64
/// and formats at the store boundary (S6 persists them as TEXT).
pub type Snowflake = u64;

/// The funnel event vocabulary: the only event types the dashboard may report.
/// Order and spelling mirror legacy `EVENT_TYPES` (two-bot `src/core/events.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "invite_click")]
    InviteClick,
    #[serde(rename = "member_join")]
    MemberJoin,
    #[serde(rename = "gate_cleared")]
    GateCleared,
    #[serde(rename = "onboarding_prompted")]
    OnboardingPrompted,
    #[serde(rename = "game_roles_selected")]
    GameRolesSelected,
    #[serde(rename = "channel_routed")]
    ChannelRouted,
    #[serde(rename = "first_message")]
    FirstMessage,
    #[serde(rename = "second_message")]
    SecondMessage,
    #[serde(rename = "third_message")]
    ThirdMessage,
    #[serde(rename = "first_voice_session")]
    FirstVoiceSession,
    #[serde(rename = "voice_session_start")]
    VoiceSessionStart,
    #[serde(rename = "voice_session_end")]
    VoiceSessionEnd,
    #[serde(rename = "member_inactive")]
    MemberInactive,
    #[serde(rename = "member_leave")]
    MemberLeave,
}

impl EventType {
    /// Legacy wire string (also the `event_type` column value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InviteClick => "invite_click",
            Self::MemberJoin => "member_join",
            Self::GateCleared => "gate_cleared",
            Self::OnboardingPrompted => "onboarding_prompted",
            Self::GameRolesSelected => "game_roles_selected",
            Self::ChannelRouted => "channel_routed",
            Self::FirstMessage => "first_message",
            Self::SecondMessage => "second_message",
            Self::ThirdMessage => "third_message",
            Self::FirstVoiceSession => "first_voice_session",
            Self::VoiceSessionStart => "voice_session_start",
            Self::VoiceSessionEnd => "voice_session_end",
            Self::MemberInactive => "member_inactive",
            Self::MemberLeave => "member_leave",
        }
    }

    /// Parse a legacy wire string (also the `event_type` column value) back
    /// into its variant. `None` for anything else: the durable read model
    /// fails closed on unknown strings rather than projecting a guess.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "invite_click" => Some(Self::InviteClick),
            "member_join" => Some(Self::MemberJoin),
            "gate_cleared" => Some(Self::GateCleared),
            "onboarding_prompted" => Some(Self::OnboardingPrompted),
            "game_roles_selected" => Some(Self::GameRolesSelected),
            "channel_routed" => Some(Self::ChannelRouted),
            "first_message" => Some(Self::FirstMessage),
            "second_message" => Some(Self::SecondMessage),
            "third_message" => Some(Self::ThirdMessage),
            "first_voice_session" => Some(Self::FirstVoiceSession),
            "voice_session_start" => Some(Self::VoiceSessionStart),
            "voice_session_end" => Some(Self::VoiceSessionEnd),
            "member_inactive" => Some(Self::MemberInactive),
            "member_leave" => Some(Self::MemberLeave),
            _ => None,
        }
    }
}

/// The message milestones in ladder order. A member's Nth message fills the
/// lowest empty rung; `second_message` exists only so the third is
/// identifiable (legacy `MESSAGE_RUNGS`).
pub const MESSAGE_RUNGS: [EventType; 3] = [
    EventType::FirstMessage,
    EventType::SecondMessage,
    EventType::ThirdMessage,
];

/// One funnel row. `occurred_at` is always set by the emitter (ISO-8601 UTC),
/// never by the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunnelEvent {
    pub guild_id: Snowflake,
    /// `None` only for `invite_click` (anonymous until a join links it).
    pub member_id: Option<Snowflake>,
    pub event_type: EventType,
    pub occurred_at: String,
    /// Attribution: `invite:CODE`, `vanity`, `ambiguous:a+b`, `unknown`,
    /// `channel:<id>`, `gateway`, `job:inactivity`, `backfill:*`, `web:one_click`.
    pub source: String,
    pub metadata: Option<serde_json::Value>,
    /// Per-request random bytes, set only by the invite-click redirect so two
    /// same-millisecond anonymous clicks do not collapse into one row.
    pub dedupe_token: Option<String>,
}

/// Repeatable event types: told apart by member + time (+ channel for voice
/// boundaries, + token for clicks). Everything else is once-per-member.
/// Mirrors legacy `idempotencyKey()`.
fn is_repeatable(t: EventType) -> bool {
    matches!(
        t,
        EventType::InviteClick
            | EventType::MemberJoin
            | EventType::MemberInactive
            | EventType::MemberLeave
            | EventType::GameRolesSelected
            | EventType::ChannelRouted
            | EventType::VoiceSessionStart
            | EventType::VoiceSessionEnd
    )
}

/// Stable key making event writes idempotent. Format is byte-identical to
/// legacy: voice boundaries carry their channel so a same-tick move end/start
/// pair does not collapse (TOG-5981).
#[must_use]
pub fn idempotency_key(e: &FunnelEvent) -> String {
    if is_repeatable(e.event_type) {
        let token = e
            .dedupe_token
            .as_deref()
            .map_or_else(String::new, |t| format!(":{t}"));
        let channel = match e.event_type {
            EventType::VoiceSessionStart | EventType::VoiceSessionEnd => {
                format!(":{}", e.source)
            }
            _ => String::new(),
        };
        let member = e
            .member_id
            .map_or_else(|| "anon".to_owned(), |m| m.to_string());
        format!(
            "{}:{}:{}:{}{}{}",
            e.guild_id,
            member,
            e.event_type.as_str(),
            e.occurred_at,
            channel,
            token
        )
    } else {
        // Once-per-member: member_id is always Some on this path.
        format!(
            "{}:{}:{}",
            e.guild_id,
            e.member_id
                .map_or_else(|| "anon".to_owned(), |m| m.to_string()),
            e.event_type.as_str()
        )
    }
}

/// Whether a `gate_cleared` row may feed time-to-clear arithmetic (TOG-6474).
/// Backfilled clearings say THAT a member is through, never WHEN: either the
/// `backfill:` source prefix or `metadata.timestampIsJoinTime` disqualifies.
/// Counting them stays fine; only time arithmetic must exclude them.
#[must_use]
pub fn is_measurable_gate_clearing(source: &str, metadata: Option<&serde_json::Value>) -> bool {
    if source.starts_with("backfill:") {
        return false;
    }
    if metadata
        .and_then(|m| m.get("timestampIsJoinTime"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return false;
    }
    true
}

// --- ISO-8601 UTC time helpers --------------------------------------------
//
// Legacy compares and stores ISO strings (`new Date().toISOString()`:
// millis precision, `Z` suffix). The core keeps that representation so row
// bytes match; parsing exists only for duration math.

/// Current time as `YYYY-MM-DDTHH:MM:SS.sssZ` (millis, like `toISOString()`).
#[must_use]
pub fn now_iso() -> String {
    format_iso_millis(now_millis())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Current epoch millis for the injectable test clock in `expected_joins`.
#[doc(hidden)]
pub fn now_millis_for_test() -> i64 {
    now_millis()
}

/// Format epoch millis as `YYYY-MM-DDTHH:MM:SS.sssZ`.
#[must_use]
pub fn format_iso_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (y, mo, d, h, mi, s) = civil_from_secs(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.frac](Z|±hh:mm)` to epoch millis.
/// Returns `None` for garbage (legacy `Date.parse` NaN path, TOG-7512).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parse_iso_millis(s: &str) -> Option<i64> {
    let t = s.trim();
    let (date, rest) = t.split_once('T').or_else(|| t.split_once('t'))?;
    let (y, mo, d) = parse_date(date)?;
    // Split off the zone: trailing Z/z, or ±hh:mm / ±hhmm / ±hh.
    let (time_part, offset_secs) = if let Some(core) = rest.strip_suffix(['Z', 'z']) {
        (core, 0_i64)
    } else {
        // Find the last + or - that starts the zone (after the time).
        let mut idx = None;
        for (i, c) in rest.char_indices().skip(8) {
            if c == '+' || c == '-' {
                idx = Some(i);
            }
        }
        let (core, zone) = rest.split_at(idx?);
        (core, parse_zone(zone)?)
    };
    let (h, mi, sec, millis) = parse_time(time_part)?;
    let days = days_from_civil(y, mo, d)?;
    let secs = days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(sec);
    Some((secs - offset_secs) * 1000 + i64::from(millis))
}

fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, mo, d))
}

fn parse_time(s: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = s.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let mi: u32 = parts.next()?.parse().ok()?;
    let (sec_part, frac) = match parts.next()? {
        with_frac if with_frac.contains(['.', ',']) => {
            let mut it = with_frac.split(['.', ',']);
            (it.next()?, Some(it.next().unwrap_or("")))
        }
        whole => (whole, None),
    };
    if parts.next().is_some() || h > 23 || mi > 59 {
        return None;
    }
    let sec: u32 = sec_part.parse().ok()?;
    if sec > 60 {
        return None;
    }
    let millis = match frac {
        None => 0,
        Some(f) => {
            if f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let padded = format!("{f:0<3}");
            padded[..3].parse().ok()?
        }
    };
    Some((h, mi, sec, millis))
}

fn parse_zone(s: &str) -> Option<i64> {
    let (sign, rest) = match s.strip_prefix('+') {
        Some(r) => (1_i64, r),
        None => (-1, s.strip_prefix('-')?),
    };
    // Digits and one `:` only. The byte slices below would land inside a
    // multi-byte character (`+1é1`) and panic, aborting the whole sweep;
    // legacy `Date.parse` is NaN, so the row is simply unparseable.
    if !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let (hh, mm): (i64, i64) = match rest.len() {
        2 => (rest.parse().ok()?, 0),
        4 => (rest[..2].parse().ok()?, rest[2..].parse().ok()?),
        5 if rest.as_bytes()[2] == b':' => (rest[..2].parse().ok()?, rest[3..].parse().ok()?),
        _ => return None,
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm). `None` if out of range.
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // The year is an unbounded `i64` from the row: bound it before the era
    // arithmetic below can overflow (a panic in debug, a wrapped instant in
    // release). 300k years comfortably contains the day bound further down.
    if y.unsigned_abs() > 300_000 {
        return None;
    }
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj.rem_euclid(400);
    let mp = (i64::from(m) + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    // Sanity: bound to ±200k years so formatting stays sane.
    if days.abs() > 73_000_000 {
        return None;
    }
    Some(days)
}

/// Inverse of `days_from_civil`: (year, month, day, hour, min, sec).
fn civil_from_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation)]
    let (h, mi, s) = (
        (sod / 3600) as u32,
        ((sod % 3600) / 60) as u32,
        (sod % 60) as u32,
    );
    (year, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_strings_match_legacy_columns() {
        assert_eq!(EventType::MemberJoin.as_str(), "member_join");
        assert_eq!(EventType::VoiceSessionEnd.as_str(), "voice_session_end");
        let json = serde_json::to_string(&EventType::GateCleared).expect("serializes");
        assert_eq!(json, "\"gate_cleared\"");
    }

    #[test]
    fn wire_strings_round_trip_and_reject_unknown() {
        for variant in [
            EventType::InviteClick,
            EventType::MemberJoin,
            EventType::GateCleared,
            EventType::OnboardingPrompted,
            EventType::GameRolesSelected,
            EventType::ChannelRouted,
            EventType::FirstMessage,
            EventType::SecondMessage,
            EventType::ThirdMessage,
            EventType::FirstVoiceSession,
            EventType::VoiceSessionStart,
            EventType::VoiceSessionEnd,
            EventType::MemberInactive,
            EventType::MemberLeave,
        ] {
            assert_eq!(EventType::from_wire(variant.as_str()), Some(variant));
        }
        assert_eq!(EventType::from_wire("landing_viewed"), None);
        assert_eq!(EventType::from_wire(""), None);
    }

    #[test]
    fn repeatable_key_carries_time_and_voice_channel() {
        let e = FunnelEvent {
            guild_id: 1,
            member_id: Some(2),
            event_type: EventType::VoiceSessionEnd,
            occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
            source: "channel:10".to_owned(),
            metadata: None,
            dedupe_token: None,
        };
        assert_eq!(
            idempotency_key(&e),
            "1:2:voice_session_end:2026-09-20T12:00:00.000Z:channel:10"
        );
    }

    #[test]
    fn once_per_member_key_ignores_time() {
        let e = FunnelEvent {
            guild_id: 1,
            member_id: Some(2),
            event_type: EventType::GateCleared,
            occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
            source: "gateway".to_owned(),
            metadata: None,
            dedupe_token: None,
        };
        assert_eq!(idempotency_key(&e), "1:2:gate_cleared");
    }

    #[test]
    fn click_key_carries_token_for_anon() {
        let e = FunnelEvent {
            guild_id: 1,
            member_id: None,
            event_type: EventType::InviteClick,
            occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
            source: "invite:abc".to_owned(),
            metadata: None,
            dedupe_token: Some("tok".to_owned()),
        };
        assert_eq!(
            idempotency_key(&e),
            "1:anon:invite_click:2026-09-20T12:00:00.000Z:tok"
        );
    }

    #[test]
    fn backfill_gate_clearing_not_measurable() {
        assert!(!is_measurable_gate_clearing("backfill:member_list", None));
        assert!(!is_measurable_gate_clearing(
            "gateway",
            Some(&serde_json::json!({"timestampIsJoinTime": true}))
        ));
        assert!(is_measurable_gate_clearing("gateway", None));
    }

    #[test]
    fn iso_round_trips() {
        for (s, ms) in [
            ("1970-01-01T00:00:00.000Z", 0),
            ("2024-09-10T20:26:40.000Z", 1_726_000_000_000),
            ("2024-09-10T20:26:40.123Z", 1_726_000_000_123),
        ] {
            assert_eq!(parse_iso_millis(s), Some(ms), "{s}");
            assert_eq!(format_iso_millis(ms), s, "{ms}");
        }
        // Offsets and offset-less fractions parse to the same instant.
        assert_eq!(
            parse_iso_millis("2024-09-10T22:26:40.000+02:00"),
            Some(1_726_000_000_000)
        );
        assert_eq!(
            parse_iso_millis("2024-09-10T20:26:40+00:00"),
            Some(1_726_000_000_000)
        );
        // Garbage is the TOG-7512 path: unmeasurable, never a throw.
        for bad in [
            "",
            "not-a-time",
            "2024-13-01T00:00:00Z",
            "2024-09-10 20:26:40",
        ] {
            assert_eq!(parse_iso_millis(bad), None, "{bad}");
        }
    }

    #[test]
    fn multibyte_zone_is_unparseable_not_a_panic() {
        // `+1é1` is a four-byte zone whose second byte sits inside `é`: the
        // old `rest[..2]` slice panicked and aborted the whole sweep.
        for bad in [
            "2026-09-20T12:00:00+1é1",
            "2026-09-20T12:00:00+0é0",
            "2026-09-20T12:00:00-1é1",
            "2026-09-20T12:00:00+é1",
            "2026-09-20T12:00:00+1é",
            "2026-09-20T12:00:00+12é",
            "2026-09-20T12:00:00+12:é",
            "2026-09-20T12:00:00+１２:００",
        ] {
            assert_eq!(parse_iso_millis(bad), None, "{bad}");
        }
        // The well-formed zone shapes still parse to the same instant.
        let utc = parse_iso_millis("2026-09-20T12:00:00Z");
        assert!(utc.is_some());
        for zone in ["+00", "+0000", "+00:00", "-00:00"] {
            assert_eq!(
                parse_iso_millis(&format!("2026-09-20T12:00:00{zone}")),
                utc,
                "{zone}"
            );
        }
        assert_eq!(
            parse_iso_millis("2026-09-20T14:00:00+0200"),
            utc,
            "offset without a colon"
        );
    }

    #[test]
    fn non_ascii_anywhere_in_a_timestamp_never_panics() {
        // Every parse helper sees a multi-byte character at each position:
        // the answer is `None` (legacy `Date.parse` NaN), never a panic.
        let base = "2026-09-20T12:00:00.123+01:00";
        for (i, _) in base.char_indices() {
            for glyph in ["é", "日", "🙂"] {
                let mut s = String::from(&base[..i]);
                s.push_str(glyph);
                s.push_str(&base[i + 1..]);
                assert_eq!(parse_iso_millis(&s), None, "{s}");
                let mut inserted = String::from(&base[..i]);
                inserted.push_str(glyph);
                inserted.push_str(&base[i..]);
                assert_eq!(parse_iso_millis(&inserted), None, "{inserted}");
            }
        }
    }

    #[test]
    fn absurd_years_are_unparseable_not_an_overflow() {
        // The year is an unbounded integer; the era arithmetic used to
        // overflow (debug panic, release wrap to a bogus instant).
        for bad in [
            "9223372036854775807-01-01T00:00:00Z",
            "9223372036854775806-06-15T00:00:00Z",
            "99999999999-01-01T00:00:00Z",
            "300001-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_iso_millis(bad), None, "{bad}");
        }
        assert!(parse_iso_millis("9999-12-31T23:59:59.999Z").is_some());
    }

    #[test]
    fn now_iso_has_toiso_shape() {
        let s = now_iso();
        assert!(s.ends_with('Z') && s.contains(".") && s.len() == 24, "{s}");
        assert!(parse_iso_millis(&s).is_some());
    }
}
