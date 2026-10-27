//! Giveaway domain: validation, eligibility, the pure draw, the status state machine.
//!
//! Slice 1 of the giveaways design (timed entry, fair draw, reroll). This
//! module is pure and database-free: entry requirements adjudication, the
//! verifiable draw, and the open/paused/drawing/drawn/cancelled lifecycle.
//! Storage lands in the `giveaway_store` module (sqlx, behind the `db`
//! feature); the command, button-entry and draw-ticker slices consume the
//! outcome enums and helpers here, so every refusal, draw and transition is
//! unit-testable without Discord or Postgres.
//!
//! Replay contract (byte-exact; anything else is a design change, not a fix
//! shape): `seed = sha256("{draw_nonce_hex}:{draw_no}")` over ASCII,
//! `eligible_hash = sha256(comma_join(sorted_user_ids))` over ASCII, and
//! winners come from a partial Fisher-Yates over the lexicographically
//! sorted eligible list with index stream
//! `u64::from_be_bytes(sha256("{seed_hex}:{i}")[..8])`. A member replays with
//! `printf '%s' 'nonce:0' | sha256sum` and a few lines of Fisher-Yates.
//! ASCII constructions are deliberate: no byte-order or hex-decoding
//! ambiguity for a verifier.
//!
//! Deliberately out of scope: the create/entry command surface, the
//! draw-ticker job, prize-claim tickets, templates, weighted bonus entries
//! (the schema reserves `weight`; the draw stays unweighted), winner roles,
//! audit exports, and behavioral anti-alt scoring (later slices).

use sha2::Digest;

/// Prize length bounds, in characters (a short prize line, not prose).
pub const PRIZE_MIN_CHARS: usize = 1;
/// Prize length bounds, in characters (fits a message line with room around it).
pub const PRIZE_MAX_CHARS: usize = 200;
/// Winner-count bounds (Giveaway Boat parity on the ceiling).
pub const WINNERS_MIN: i64 = 1;
/// Winner-count bounds (Giveaway Boat parity on the ceiling).
pub const WINNERS_MAX: i64 = 100;
/// Shortest giveaway: a minute, so a deadline never races the 15–30s draw ticker.
pub const DURATION_MIN_SECONDS: i64 = 60;
/// Longest giveaway: 30 days, bounding storage and the due scan.
pub const DURATION_MAX_SECONDS: i64 = 2_592_000;
/// Shortest deadline extension: one second (extending is not re-creating).
pub const EXTEND_MIN_SECONDS: i64 = 1;
/// Longest deadline extension: the same 30-day ceiling as a fresh duration.
pub const EXTEND_MAX_SECONDS: i64 = DURATION_MAX_SECONDS;
/// Eligibility age gates cap at ten years; beyond that the gate is a ban, not a filter.
pub const REQUIREMENT_MAX_AGE_DAYS: u32 = 3_650;
/// First draw of a giveaway is draw 0; each reroll inserts `draw_no + 1`.
pub const FIRST_DRAW_NO: i64 = 0;
/// `reroll_kind` for a full redraw (the first draw stores NULL instead).
pub const REROLL_KIND_FULL: &str = "full";

/// A giveaway lifecycle status (the `giveaways.status` column vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GiveawayStatus {
    Open,
    Paused,
    Drawing,
    Drawn,
    Cancelled,
}

impl GiveawayStatus {
    /// All five statuses in lifecycle order.
    pub const ALL: [Self; 5] = [
        Self::Open,
        Self::Paused,
        Self::Drawing,
        Self::Drawn,
        Self::Cancelled,
    ];

    /// Wire value (`open`, … — the stored column text).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Paused => "paused",
            Self::Drawing => "drawing",
            Self::Drawn => "drawn",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a wire value. Anything outside the five stored statuses is a
    /// caller bug or a corrupt row, surfaced as [`GiveawayError::UnknownStatus`].
    pub fn parse(value: &str) -> Result<Self, GiveawayError> {
        match value {
            "open" => Ok(Self::Open),
            "paused" => Ok(Self::Paused),
            "drawing" => Ok(Self::Drawing),
            "drawn" => Ok(Self::Drawn),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(GiveawayError::UnknownStatus(other.to_owned())),
        }
    }
}

impl std::fmt::Display for GiveawayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A lifecycle event the router or ticker applies to a giveaway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GiveawayEvent {
    Pause,
    Resume,
    BeginDraw,
    CompleteDraw,
    Cancel,
}

impl GiveawayEvent {
    /// Verb phrase for refusal text (`pause`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::BeginDraw => "begin the draw for",
            Self::CompleteDraw => "complete the draw for",
            Self::Cancel => "cancel",
        }
    }
}

impl std::fmt::Display for GiveawayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Invalid giveaway input or an illegal lifecycle step.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GiveawayError {
    #[error("prize must be 1–200 characters.")]
    InvalidPrize,
    #[error("winners must be 1–100.")]
    InvalidWinnersCount,
    #[error("duration must be 60–2592000 seconds.")]
    InvalidDuration,
    #[error("extensions must be 1–2592000 seconds.")]
    InvalidExtension,
    #[error("unknown giveaway status {0:?}: expected open, paused, drawing, drawn or cancelled.")]
    UnknownStatus(String),
    #[error("cannot {1} a {0} giveaway.")]
    InvalidTransition(GiveawayStatus, GiveawayEvent),
    #[error("draw nonces are 128-bit hex.")]
    InvalidDrawNonce,
    #[error("giveaway requirements are not valid: {0}.")]
    InvalidRequirements(String),
    #[error("unknown reroll kind {0:?}: expected full or single:<user id>.")]
    InvalidRerollKind(String),
}

/// Validate a prize line: trimmed, 1–200 characters. Returns the trimmed text.
pub fn validate_prize(prize: &str) -> Result<String, GiveawayError> {
    let trimmed = prize.trim().to_owned();
    if (PRIZE_MIN_CHARS..=PRIZE_MAX_CHARS).contains(&trimmed.chars().count()) {
        Ok(trimmed)
    } else {
        Err(GiveawayError::InvalidPrize)
    }
}

/// Validate a winner count: 1–100.
pub fn validate_winners_count(count: i64) -> Result<i64, GiveawayError> {
    if (WINNERS_MIN..=WINNERS_MAX).contains(&count) {
        Ok(count)
    } else {
        Err(GiveawayError::InvalidWinnersCount)
    }
}

/// Validate a giveaway duration in seconds: 60 through 30 days.
pub fn validate_duration_seconds(seconds: i64) -> Result<i64, GiveawayError> {
    if (DURATION_MIN_SECONDS..=DURATION_MAX_SECONDS).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(GiveawayError::InvalidDuration)
    }
}

/// Unvalidated `/giveaway create` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGiveawayInput {
    pub prize: String,
    pub winners_count: i64,
    pub duration_seconds: i64,
}

/// A create input whose prize, winner count and duration all passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedGiveaway {
    pub prize: String,
    pub winners_count: i64,
    pub duration_seconds: i64,
}

/// Validate a create input, checking prize first, then winner count, then duration.
pub fn validate_new_giveaway(input: &NewGiveawayInput) -> Result<ValidatedGiveaway, GiveawayError> {
    Ok(ValidatedGiveaway {
        prize: validate_prize(&input.prize)?,
        winners_count: validate_winners_count(input.winners_count)?,
        duration_seconds: validate_duration_seconds(input.duration_seconds)?,
    })
}

/// Entry requirements (the parsed `requirements_json` column). Every field
/// defaults, so `"{}"` means "every member may enter". Unknown fields are
/// refused: a misspelled key must fail loudly instead of silently dropping
/// a gate the moderator thought they set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GiveawayRequirements {
    /// Required role ids (snowflakes): the member must hold every one.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Minimum Discord account age in days.
    #[serde(default)]
    pub min_account_age_days: u32,
    /// Minimum time-in-server in days.
    #[serde(default)]
    pub min_guild_age_days: u32,
}

/// Parse and validate stored requirements: well-formed JSON, snowflake role
/// ids, and age gates within ten years.
pub fn parse_requirements(json: &str) -> Result<GiveawayRequirements, GiveawayError> {
    let requirements: GiveawayRequirements = serde_json::from_str(json)
        .map_err(|error| GiveawayError::InvalidRequirements(error.to_string()))?;
    for role in &requirements.roles {
        if !crate::rsvp::is_snowflake(role) {
            return Err(GiveawayError::InvalidRequirements(format!(
                "role {role:?} is not a Discord id"
            )));
        }
    }
    for (name, days) in [
        ("min_account_age_days", requirements.min_account_age_days),
        ("min_guild_age_days", requirements.min_guild_age_days),
    ] {
        if days > REQUIREMENT_MAX_AGE_DAYS {
            return Err(GiveawayError::InvalidRequirements(format!(
                "{name} {days} exceeds ten years"
            )));
        }
    }
    Ok(requirements)
}

/// A member's standing against the entry requirements: held roles plus the
/// `join_risk_store`/`membership` age evidence, in whole days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberStanding {
    pub role_ids: Vec<String>,
    pub account_age_days: u32,
    pub guild_age_days: u32,
}

/// Eligibility adjudication outcome, checked roles first, then account age,
/// then time-in-server, so the first refusal names the first unmet gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eligibility {
    Eligible,
    MissingRoles,
    AccountTooNew,
    GuildTooNew,
}

/// Adjudicate one member against the requirements. Re-checked at entry time
/// and again inside the draw transaction: leaving the server or losing a
/// role between entry and draw removes the member from the eligible list.
#[must_use]
pub fn adjudicate_eligibility(
    requirements: &GiveawayRequirements,
    member: &MemberStanding,
) -> Eligibility {
    if !requirements
        .roles
        .iter()
        .all(|role| member.role_ids.contains(role))
    {
        return Eligibility::MissingRoles;
    }
    if member.account_age_days < requirements.min_account_age_days {
        return Eligibility::AccountTooNew;
    }
    if member.guild_age_days < requirements.min_guild_age_days {
        return Eligibility::GuildTooNew;
    }
    Eligibility::Eligible
}

/// Apply one lifecycle event. Open pauses and draws; paused resumes (the
/// ticker extends the deadline by the paused span) or cancels; drawing
/// completes; open or paused cancels. A draw in flight is never cancelled —
/// it completes or fails first — and drawn/cancelled are terminal.
pub fn transition(
    status: GiveawayStatus,
    event: GiveawayEvent,
) -> Result<GiveawayStatus, GiveawayError> {
    match (status, event) {
        (GiveawayStatus::Open, GiveawayEvent::Pause) => Ok(GiveawayStatus::Paused),
        (GiveawayStatus::Paused, GiveawayEvent::Resume) => Ok(GiveawayStatus::Open),
        (GiveawayStatus::Open, GiveawayEvent::BeginDraw) => Ok(GiveawayStatus::Drawing),
        (GiveawayStatus::Drawing, GiveawayEvent::CompleteDraw) => Ok(GiveawayStatus::Drawn),
        (GiveawayStatus::Open | GiveawayStatus::Paused, GiveawayEvent::Cancel) => {
            Ok(GiveawayStatus::Cancelled)
        }
        _ => Err(GiveawayError::InvalidTransition(status, event)),
    }
}

/// Resume math: the deadline moves forward by the paused span, so pausing
/// never shortens entry. A backwards clock resumes without moving the
/// deadline, and the addition saturates instead of overflowing.
#[must_use]
pub fn resume_deadline_ms(ends_at_ms: i64, paused_at_ms: i64, resumed_at_ms: i64) -> i64 {
    ends_at_ms.saturating_add(resumed_at_ms.saturating_sub(paused_at_ms).max(0))
}

/// Extend the deadline by `extra_seconds` (1 through 30 days). The addition
/// saturates instead of overflowing.
pub fn extend_deadline_ms(ends_at_ms: i64, extra_seconds: i64) -> Result<i64, GiveawayError> {
    if !(EXTEND_MIN_SECONDS..=EXTEND_MAX_SECONDS).contains(&extra_seconds) {
        return Err(GiveawayError::InvalidExtension);
    }
    Ok(ends_at_ms.saturating_add(extra_seconds.saturating_mul(1_000)))
}

/// True when the draw ticker owns this giveaway now: still open and past its deadline.
#[must_use]
pub fn is_due(status: GiveawayStatus, ends_at_ms: i64, now_ms: i64) -> bool {
    status == GiveawayStatus::Open && now_ms >= ends_at_ms
}

fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_ref());
    hex::encode(hasher.finalize())
}

fn sha256_raw(input: &str) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    hasher.finalize().into()
}

/// Mint a 128-bit draw nonce (lowercase hex) for a new giveaway. The nonce
/// is secret until the draw, then revealed in the announcement so any member
/// can replay the seed.
#[must_use]
pub fn mint_draw_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Derive the published draw seed: `sha256("{draw_nonce_hex}:{draw_no}")`
/// over ASCII. Each round's seed differs, so rerolls decorrelate.
pub fn draw_seed(draw_nonce_hex: &str, draw_no: i64) -> Result<String, GiveawayError> {
    if draw_nonce_hex.len() != 32 || !draw_nonce_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GiveawayError::InvalidDrawNonce);
    }
    Ok(sha256_hex(format!("{draw_nonce_hex}:{draw_no}")))
}

/// Commit to the eligible list: `sha256` over the ASCII comma-join of the
/// lexicographically sorted eligible user ids. Sorting inside makes the
/// canonical order part of the contract: entry order never affects the hash.
#[must_use]
pub fn eligible_hash(eligible_user_ids: &[String]) -> String {
    let mut sorted = eligible_user_ids.to_vec();
    sorted.sort();
    sha256_hex(sorted.join(","))
}

/// The pure draw: partial Fisher-Yates over the lexicographically sorted
/// eligible list. Position `i` swaps with `i + (stream(i) mod (n - i))`
/// where `stream(i)` is the first 8 bytes, big-endian, of
/// `sha256("{seed_hex}:{i}")`. Total over edge cases: zero entrants draws
/// nobody, and fewer entrants than winners draws everybody in draw order.
#[must_use]
pub fn draw(eligible_user_ids: &[String], seed_hex: &str, winners_count: usize) -> Vec<String> {
    let mut order = eligible_user_ids.to_vec();
    order.sort();
    let total = order.len();
    let take = winners_count.min(total);
    for i in 0..take {
        let digest = sha256_raw(&format!("{seed_hex}:{i}"));
        let rand = u64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ]);
        let pick = i + (rand % (total - i) as u64) as usize;
        order.swap(i, pick);
    }
    order.truncate(take);
    order
}

/// The eligible pool for a single-winner reroll: everybody except the kept
/// winners. Draw one from the pool with the next round seed.
#[must_use]
pub fn single_reroll_pool(eligible_user_ids: &[String], keep_winner_ids: &[String]) -> Vec<String> {
    eligible_user_ids
        .iter()
        .filter(|id| !keep_winner_ids.contains(id))
        .cloned()
        .collect()
}

/// Winners to hold out of a single-winner reroll pool: every latest winner,
/// so the replacement is always fresh. When that empties the pool (every
/// entrant already won), hold out only the replaced member and re-pick among
/// the rest; when even that is empty the caller draws nobody.
#[must_use]
pub fn single_reroll_keep(
    eligible_user_ids: &[String],
    latest_winner_ids: &[String],
    replaced_user_id: &str,
) -> Vec<String> {
    if single_reroll_pool(eligible_user_ids, latest_winner_ids).is_empty() {
        vec![replaced_user_id.to_owned()]
    } else {
        latest_winner_ids.to_vec()
    }
}

/// The full winner set after replacing one winner: the latest winners in
/// order, with `replaced_user_id` swapped for the drawn replacement (or simply
/// removed when nobody was drawn). Kept winners always survive and no member
/// holds two seats, even when the full-house fallback of
/// [`single_reroll_keep`] re-picked an existing winner.
#[must_use]
pub fn single_reroll_winners(
    latest_winner_ids: &[String],
    replaced_user_id: &str,
    replacement_ids: &[String],
) -> Vec<String> {
    let mut next: Vec<String> = Vec::with_capacity(latest_winner_ids.len());
    for winner in latest_winner_ids {
        let slot = if winner == replaced_user_id {
            replacement_ids
        } else {
            std::slice::from_ref(winner)
        };
        for id in slot {
            if !next.contains(id) {
                next.push(id.clone());
            }
        }
    }
    next
}

/// Render the `reroll_kind` for replacing one winner: `single:<user id>`.
#[must_use]
pub fn reroll_kind_single(user_id: &str) -> String {
    format!("single:{user_id}")
}

/// A stored draw round: the first draw (NULL `reroll_kind`), a full redraw,
/// or the replacement of one winner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RerollKind {
    Initial,
    Full,
    Single(String),
}

/// Parse a stored `reroll_kind`: NULL is the first draw, `full` a redraw,
/// `single:<user id>` a one-winner replacement.
pub fn parse_reroll_kind(value: Option<&str>) -> Result<RerollKind, GiveawayError> {
    match value {
        None => Ok(RerollKind::Initial),
        Some(REROLL_KIND_FULL) => Ok(RerollKind::Full),
        Some(single) => match single.strip_prefix("single:") {
            Some(id) if !id.is_empty() => Ok(RerollKind::Single(id.to_owned())),
            _ => Err(GiveawayError::InvalidRerollKind(single.to_owned())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed 128-bit draw nonce (hex) for the reference vectors.
    const VECTOR_NONCE: &str = "9f2c4a7e1b5d8f03a6c9e2b4d7f1a83c";
    /// Ten fixed eligible member ids for the reference vectors.
    const VECTOR_ELIGIBLE: [&str; 10] = [
        "100000000000000001",
        "100000000000000002",
        "100000000000000003",
        "100000000000000004",
        "100000000000000005",
        "100000000000000006",
        "100000000000000007",
        "100000000000000008",
        "100000000000000009",
        "100000000000000010",
    ];
    /// `draw_seed(VECTOR_NONCE, 0)`.
    const VECTOR_SEED_0: &str = "fdab72cb4823dbf292f0ed3fea3abef20dc268169a90f6aea0a1cd4c1a8c15ab";
    /// `draw_seed(VECTOR_NONCE, 1)` — the full-reroll seed.
    const VECTOR_SEED_1: &str = "16688231019cd5f768dcb0e2a604881b365c0ba7b242433b3e7ff96bde0bdf88";
    /// `eligible_hash(VECTOR_ELIGIBLE)`.
    const VECTOR_ELIGIBLE_HASH: &str =
        "deebe3bffe4ba1543bd568b42a31508a96ef5c0ba98464a3f1e572286ead0299";
    /// `draw(VECTOR_ELIGIBLE, VECTOR_SEED_0, 2)`.
    const VECTOR_WINNERS_10_OF_2: [&str; 2] = ["100000000000000009", "100000000000000002"];
    /// `draw(first 3 of VECTOR_ELIGIBLE, VECTOR_SEED_0, 5)`.
    const VECTOR_WINNERS_3_OF_5: [&str; 3] = [
        "100000000000000001",
        "100000000000000003",
        "100000000000000002",
    ];
    /// `draw(VECTOR_ELIGIBLE, VECTOR_SEED_1, 2)` — the full reroll.
    const VECTOR_REROLL_WINNERS: [&str; 2] = ["100000000000000005", "100000000000000002"];

    fn eligible() -> Vec<String> {
        VECTOR_ELIGIBLE.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn status_vocabulary_round_trips() {
        assert_eq!(GiveawayStatus::ALL.len(), 5);
        for status in GiveawayStatus::ALL {
            assert_eq!(GiveawayStatus::parse(status.as_str()), Ok(status));
            assert_eq!(status.to_string(), status.as_str());
        }
        assert_eq!(GiveawayStatus::parse("open"), Ok(GiveawayStatus::Open));
        for bogus in ["", "OPEN", "ended", "deleted"] {
            assert_eq!(
                GiveawayStatus::parse(bogus),
                Err(GiveawayError::UnknownStatus(bogus.to_owned())),
                "{bogus:?} is outside the status vocabulary"
            );
        }
    }

    #[test]
    fn events_render_for_refusal_text() {
        assert_eq!(GiveawayEvent::Pause.to_string(), "pause");
        assert_eq!(GiveawayEvent::Resume.to_string(), "resume");
        assert_eq!(GiveawayEvent::BeginDraw.to_string(), "begin the draw for");
        assert_eq!(
            GiveawayEvent::CompleteDraw.to_string(),
            "complete the draw for"
        );
        assert_eq!(GiveawayEvent::Cancel.to_string(), "cancel");
    }

    #[test]
    fn lifecycle_transitions_follow_the_state_machine() {
        use GiveawayEvent::{BeginDraw, Cancel, CompleteDraw, Pause, Resume};
        use GiveawayStatus::{Cancelled, Drawing, Drawn, Open, Paused};
        assert_eq!(transition(Open, Pause), Ok(Paused));
        assert_eq!(transition(Paused, Resume), Ok(Open));
        assert_eq!(transition(Open, BeginDraw), Ok(Drawing));
        assert_eq!(transition(Drawing, CompleteDraw), Ok(Drawn));
        assert_eq!(transition(Open, Cancel), Ok(Cancelled));
        assert_eq!(transition(Paused, Cancel), Ok(Cancelled));
        for (status, event) in [
            (Open, Resume),
            (Open, CompleteDraw),
            (Paused, Pause),
            (Paused, BeginDraw),
            (Drawing, Pause),
            (Drawing, Resume),
            (Drawing, Cancel),
            (Drawing, BeginDraw),
            (Drawn, Pause),
            (Drawn, Cancel),
            (Cancelled, Resume),
            (Cancelled, Cancel),
        ] {
            assert_eq!(
                transition(status, event),
                Err(GiveawayError::InvalidTransition(status, event)),
                "cannot {event} a {status} giveaway"
            );
        }
        assert_eq!(
            GiveawayError::InvalidTransition(Drawn, Resume).to_string(),
            "cannot resume a drawn giveaway."
        );
    }

    #[test]
    fn prize_bounds_count_trimmed_characters() {
        assert_eq!(
            validate_prize("Two game keys"),
            Ok("Two game keys".to_owned())
        );
        assert_eq!(validate_prize("  padded  "), Ok("padded".to_owned()));
        assert_eq!(validate_prize("x"), Ok("x".to_owned()));
        let max = "é".repeat(PRIZE_MAX_CHARS);
        assert_eq!(validate_prize(&max), Ok(max));
        assert_eq!(validate_prize(""), Err(GiveawayError::InvalidPrize));
        assert_eq!(validate_prize("   "), Err(GiveawayError::InvalidPrize));
        assert_eq!(
            validate_prize(&"y".repeat(PRIZE_MAX_CHARS + 1)),
            Err(GiveawayError::InvalidPrize)
        );
    }

    #[test]
    fn winner_count_bounds_are_1_to_100() {
        assert_eq!(validate_winners_count(1), Ok(1));
        assert_eq!(validate_winners_count(100), Ok(100));
        for bad in [0, -1, 101, i64::MAX] {
            assert_eq!(
                validate_winners_count(bad),
                Err(GiveawayError::InvalidWinnersCount)
            );
        }
    }

    #[test]
    fn duration_bounds_are_a_minute_through_30_days() {
        assert_eq!(validate_duration_seconds(60), Ok(60));
        assert_eq!(
            validate_duration_seconds(DURATION_MAX_SECONDS),
            Ok(DURATION_MAX_SECONDS)
        );
        for bad in [0, 59, -60, DURATION_MAX_SECONDS + 1] {
            assert_eq!(
                validate_duration_seconds(bad),
                Err(GiveawayError::InvalidDuration)
            );
        }
    }

    #[test]
    fn create_validation_checks_prize_winners_then_duration() {
        let valid = NewGiveawayInput {
            prize: "Two game keys".to_owned(),
            winners_count: 2,
            duration_seconds: 3_600,
        };
        assert_eq!(
            validate_new_giveaway(&valid),
            Ok(ValidatedGiveaway {
                prize: "Two game keys".to_owned(),
                winners_count: 2,
                duration_seconds: 3_600,
            })
        );
        let bad_prize = NewGiveawayInput {
            prize: String::new(),
            ..valid.clone()
        };
        assert_eq!(
            validate_new_giveaway(&bad_prize),
            Err(GiveawayError::InvalidPrize)
        );
        let bad_winners = NewGiveawayInput {
            winners_count: 0,
            ..valid.clone()
        };
        assert_eq!(
            validate_new_giveaway(&bad_winners),
            Err(GiveawayError::InvalidWinnersCount)
        );
        let bad_duration = NewGiveawayInput {
            duration_seconds: 59,
            ..valid.clone()
        };
        assert_eq!(
            validate_new_giveaway(&bad_duration),
            Err(GiveawayError::InvalidDuration)
        );
    }

    #[test]
    fn requirements_default_open_and_validate() {
        assert_eq!(
            parse_requirements("{}"),
            Ok(GiveawayRequirements {
                roles: Vec::new(),
                min_account_age_days: 0,
                min_guild_age_days: 0,
            })
        );
        let parsed = parse_requirements(
            r#"{"roles":["100000000000000001"],"min_account_age_days":30,"min_guild_age_days":7}"#,
        )
        .expect("valid requirements");
        assert_eq!(parsed.roles, ["100000000000000001"]);
        assert_eq!(parsed.min_account_age_days, 30);
        assert_eq!(parsed.min_guild_age_days, 7);
        let boundary =
            parse_requirements(r#"{"min_account_age_days":3650,"min_guild_age_days":3650}"#)
                .expect("ten-year gates are the ceiling");
        assert_eq!(boundary.min_account_age_days, 3_650);
        assert!(matches!(
            parse_requirements("{oops"),
            Err(GiveawayError::InvalidRequirements(_))
        ));
        assert!(matches!(
            parse_requirements(r#"{"roles":["not-a-snowflake"]}"#),
            Err(GiveawayError::InvalidRequirements(_))
        ));
        assert!(matches!(
            parse_requirements(r#"{"min_account_age_days":3651}"#),
            Err(GiveawayError::InvalidRequirements(_))
        ));
        assert!(matches!(
            parse_requirements(r#"{"min_guild_age_days":3651}"#),
            Err(GiveawayError::InvalidRequirements(_))
        ));
        assert!(matches!(
            parse_requirements(r#"{"min_account_age_days":30,"min_acount_age_days":7}"#),
            Err(GiveawayError::InvalidRequirements(_))
        ));
    }

    #[test]
    fn eligibility_adjudication_checks_roles_then_ages() {
        let open = GiveawayRequirements {
            roles: Vec::new(),
            min_account_age_days: 0,
            min_guild_age_days: 0,
        };
        let newcomer = MemberStanding {
            role_ids: Vec::new(),
            account_age_days: 0,
            guild_age_days: 0,
        };
        assert_eq!(
            adjudicate_eligibility(&open, &newcomer),
            Eligibility::Eligible
        );
        let gated = GiveawayRequirements {
            roles: vec![
                "100000000000000001".to_owned(),
                "100000000000000002".to_owned(),
            ],
            min_account_age_days: 30,
            min_guild_age_days: 7,
        };
        let elder = MemberStanding {
            role_ids: vec![
                "100000000000000001".to_owned(),
                "100000000000000002".to_owned(),
            ],
            account_age_days: 30,
            guild_age_days: 7,
        };
        assert_eq!(
            adjudicate_eligibility(&gated, &elder),
            Eligibility::Eligible
        );
        let missing_one = MemberStanding {
            role_ids: vec!["100000000000000001".to_owned()],
            account_age_days: 365,
            guild_age_days: 365,
        };
        assert_eq!(
            adjudicate_eligibility(&gated, &missing_one),
            Eligibility::MissingRoles
        );
        let young_account = MemberStanding {
            role_ids: elder.role_ids.clone(),
            account_age_days: 29,
            guild_age_days: 365,
        };
        assert_eq!(
            adjudicate_eligibility(&gated, &young_account),
            Eligibility::AccountTooNew
        );
        let young_member = MemberStanding {
            role_ids: elder.role_ids.clone(),
            account_age_days: 365,
            guild_age_days: 6,
        };
        assert_eq!(
            adjudicate_eligibility(&gated, &young_member),
            Eligibility::GuildTooNew
        );
        let failing_everything = MemberStanding {
            role_ids: Vec::new(),
            account_age_days: 0,
            guild_age_days: 0,
        };
        assert_eq!(
            adjudicate_eligibility(&gated, &failing_everything),
            Eligibility::MissingRoles,
            "roles refuse before ages"
        );
    }

    #[test]
    fn resume_extends_the_deadline_by_the_paused_span() {
        assert_eq!(resume_deadline_ms(1_000, 100, 300), 1_200);
        assert_eq!(
            resume_deadline_ms(1_000, 300, 100),
            1_000,
            "a backwards clock resumes without moving the deadline"
        );
        assert_eq!(resume_deadline_ms(i64::MAX, 0, 10), i64::MAX);
    }

    #[test]
    fn extensions_are_bounded_and_saturate() {
        assert_eq!(extend_deadline_ms(1_000, 1), Ok(2_000));
        assert_eq!(
            extend_deadline_ms(0, EXTEND_MAX_SECONDS),
            Ok(EXTEND_MAX_SECONDS * 1_000)
        );
        assert_eq!(extend_deadline_ms(i64::MAX, 1), Ok(i64::MAX));
        for bad in [0, -1, EXTEND_MAX_SECONDS + 1] {
            assert_eq!(
                extend_deadline_ms(1_000, bad),
                Err(GiveawayError::InvalidExtension)
            );
        }
    }

    #[test]
    fn only_open_past_deadline_giveaways_are_due() {
        assert!(is_due(GiveawayStatus::Open, 1_000, 1_000));
        assert!(is_due(GiveawayStatus::Open, 1_000, 2_000));
        assert!(!is_due(GiveawayStatus::Open, 2_000, 1_000));
        assert!(!is_due(GiveawayStatus::Paused, 1_000, 2_000));
        assert!(!is_due(GiveawayStatus::Drawing, 1_000, 2_000));
        assert!(!is_due(GiveawayStatus::Drawn, 1_000, 2_000));
        assert!(!is_due(GiveawayStatus::Cancelled, 1_000, 2_000));
    }

    #[test]
    fn minted_nonces_are_128_bit_hex_and_seed() {
        let nonce = mint_draw_nonce();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            nonce.bytes().all(|byte| !byte.is_ascii_uppercase()),
            "minted nonces are lowercase hex"
        );
        assert!(draw_seed(&nonce, FIRST_DRAW_NO).is_ok());
    }

    #[test]
    fn seeds_derive_per_round_and_reject_bad_nonces() {
        assert_eq!(draw_seed(VECTOR_NONCE, 0), Ok(VECTOR_SEED_0.to_owned()));
        assert_eq!(draw_seed(VECTOR_NONCE, 1), Ok(VECTOR_SEED_1.to_owned()));
        assert_ne!(VECTOR_SEED_0, VECTOR_SEED_1);
        assert!(draw_seed(&VECTOR_NONCE.to_ascii_uppercase(), 0).is_ok());
        for bad in [
            "9f2c4a7e1b5d8f03a6c9e2b4d7f1a83",   // 31 chars
            "9f2c4a7e1b5d8f03a6c9e2b4d7f1a83c0", // 33 chars
            "zf2c4a7e1b5d8f03a6c9e2b4d7f1a83c",  // not hex
            "",
        ] {
            assert_eq!(
                draw_seed(bad, 0),
                Err(GiveawayError::InvalidDrawNonce),
                "{bad:?} is not a 128-bit hex nonce"
            );
        }
    }

    #[test]
    fn eligible_hash_commits_to_the_sorted_list() {
        assert_eq!(eligible_hash(&eligible()), VECTOR_ELIGIBLE_HASH);
        let mut shuffled = eligible();
        shuffled.reverse();
        assert_eq!(
            eligible_hash(&shuffled),
            VECTOR_ELIGIBLE_HASH,
            "entry order never affects the eligible hash"
        );
        let mut tampered = eligible();
        tampered.pop();
        assert_ne!(
            eligible_hash(&tampered),
            VECTOR_ELIGIBLE_HASH,
            "the hash commits to the full eligible list"
        );
        assert_ne!(
            eligible_hash(&[]),
            VECTOR_ELIGIBLE_HASH,
            "the empty list hashes distinctly"
        );
    }

    #[test]
    fn draw_replays_the_reference_vectors() {
        let winners = draw(&eligible(), VECTOR_SEED_0, 2);
        assert_eq!(
            winners,
            VECTOR_WINNERS_10_OF_2
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            draw(&eligible(), VECTOR_SEED_0, 2),
            winners,
            "the same seed replays to the same winners"
        );
        let short: Vec<String> = eligible().into_iter().take(3).collect();
        assert_eq!(
            draw(&short, VECTOR_SEED_0, 5),
            VECTOR_WINNERS_3_OF_5
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "fewer entrants than winners draws everybody, in draw order"
        );
        assert!(
            draw(&[], VECTOR_SEED_0, 2).is_empty(),
            "zero entrants draws nobody"
        );
        assert!(
            draw(&eligible(), VECTOR_SEED_0, 0).is_empty(),
            "drawing zero winners draws nobody"
        );
        assert_eq!(
            draw(&eligible(), VECTOR_SEED_1, 2),
            VECTOR_REROLL_WINNERS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "a full reroll replays from the next round seed"
        );
        assert_ne!(
            draw(&eligible(), VECTOR_SEED_0, 2),
            draw(&eligible(), VECTOR_SEED_1, 2),
            "distinct round seeds must decorrelate the fixed vectors"
        );
        let mut shuffled = eligible();
        shuffled.reverse();
        assert_eq!(
            draw(&shuffled, VECTOR_SEED_0, 2),
            winners,
            "entry order never affects the draw"
        );
    }

    #[test]
    fn single_reroll_draws_one_outside_the_kept_winners() {
        let kept = vec![VECTOR_WINNERS_10_OF_2[0].to_owned()];
        let pool = single_reroll_pool(&eligible(), &kept);
        assert_eq!(pool.len(), 9);
        assert!(!pool.contains(&kept[0]));
        let replacement = draw(&pool, VECTOR_SEED_1, 1);
        assert_eq!(replacement.len(), 1);
        assert!(!kept.contains(&replacement[0]));
        assert_eq!(
            draw(&pool, VECTOR_SEED_1, 1),
            replacement,
            "the single reroll replays deterministically"
        );
        assert!(single_reroll_pool(&[], &kept).is_empty());
        assert_eq!(single_reroll_pool(&eligible(), &[]).len(), 10);
    }

    #[test]
    fn single_reroll_keep_holds_every_winner_until_the_pool_runs_dry() {
        let alice = "100000000000000001".to_owned();
        let bob = "100000000000000002".to_owned();
        let eligible = vec![alice.clone(), bob.clone()];
        // Two entrants, one winner: keeping the replaced winner leaves the
        // other member as the only candidate, so the replacement is fresh.
        let keep = single_reroll_keep(&eligible, std::slice::from_ref(&alice), &alice);
        assert_eq!(keep, vec![alice.clone()]);
        let pool = single_reroll_pool(&eligible, &keep);
        assert_eq!(pool, vec![bob.clone()]);
        assert_eq!(draw(&pool, VECTOR_SEED_1, 1), vec![bob.clone()]);
        // Full house: every entrant already won, so the fallback keeps only
        // the replaced member out and the draw re-picks among the rest.
        let winners = vec![alice.clone(), bob.clone()];
        let keep = single_reroll_keep(&eligible, &winners, &alice);
        assert_eq!(keep, vec![alice.clone()]);
        let pool = single_reroll_pool(&eligible, &keep);
        assert_eq!(pool, vec![bob.clone()]);
        // Lone winner: nobody left to draw, so the reroll seats nobody.
        let solo = vec![alice.clone()];
        let keep = single_reroll_keep(&solo, std::slice::from_ref(&alice), &alice);
        assert_eq!(keep, vec![alice.clone()]);
        assert!(single_reroll_pool(&solo, &keep).is_empty());
        assert!(draw(&single_reroll_pool(&solo, &keep), VECTOR_SEED_1, 1).is_empty());
    }

    #[test]
    fn single_reroll_winners_keep_the_full_set_without_duplicates() {
        let ids: Vec<String> = (1..=4).map(|n| format!("10000000000000000{n}")).collect();
        let (a, b, c, d) = (&ids[0], &ids[1], &ids[2], &ids[3]);
        let latest = vec![a.clone(), b.clone()];
        // Replacing A with fresh C keeps B and seats C in A's slot.
        assert_eq!(
            single_reroll_winners(&latest, a, std::slice::from_ref(c)),
            vec![c.clone(), b.clone()]
        );
        // A kept winner stays targetable on the next round.
        let next = single_reroll_winners(&latest, a, std::slice::from_ref(c));
        assert_eq!(
            single_reroll_winners(&next, b, std::slice::from_ref(d)),
            vec![c.clone(), d.clone()]
        );
        // Nobody drawn: the replaced winner is removed, the rest survive.
        assert_eq!(single_reroll_winners(&latest, a, &[]), vec![b.clone()]);
        // Full-house fallback re-picked an existing winner: no double seat,
        // whichever side of the target the duplicate sits on.
        assert_eq!(
            single_reroll_winners(&latest, a, std::slice::from_ref(b)),
            vec![b.clone()]
        );
        assert_eq!(
            single_reroll_winners(&latest, b, std::slice::from_ref(a)),
            vec![a.clone()]
        );
    }

    #[test]
    fn reroll_kinds_cover_initial_full_and_single() {
        assert_eq!(REROLL_KIND_FULL, "full");
        assert_eq!(parse_reroll_kind(None), Ok(RerollKind::Initial));
        assert_eq!(parse_reroll_kind(Some("full")), Ok(RerollKind::Full));
        assert_eq!(
            reroll_kind_single("100000000000000008"),
            "single:100000000000000008"
        );
        assert_eq!(
            parse_reroll_kind(Some("single:100000000000000008")),
            Ok(RerollKind::Single("100000000000000008".to_owned()))
        );
        for bogus in ["", "FULL", "full:x", "single:", "double:1"] {
            assert_eq!(
                parse_reroll_kind(Some(bogus)),
                Err(GiveawayError::InvalidRerollKind(bogus.to_owned())),
                "{bogus:?} is outside the reroll vocabulary"
            );
        }
    }
}
