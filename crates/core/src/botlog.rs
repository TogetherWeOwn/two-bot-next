//! Publishable-kind projection for the keyless `botlog.read` verb (contract
//! v2 §6, card B6): the public bot log is not a table but a closed projection
//! over `internal_action_log`, enforced where the audit is written (the
//! `member_milestones` whitelist precedent). Anything not listed here does
//! not exist as far as the public log is concerned.
//!
//! Publishable rows are terminal successes only (`phase = 'terminal'` with a
//! success `response_code`); failures, refusals and in-flight intents never
//! publish. Per kind:
//!
//! - `event.opened` / `event.updated`: an `event.upsert` whose outcome is
//!   `created` / `updated`. The event is named (`event_key`), never the
//!   actor, so there is no subject to suppress.
//! - `event.cancelled`: an `event.cancel` whose outcome is `cancelled`.
//! - `announcement.posted`: an `announcement.post`. The channel is named by
//!   its configured key only (`detail`), never the body text. The key
//!   resolves through the receiver's channel map; a target that no longer
//!   maps to a configured key fails closed (no entry) rather than leaking a
//!   raw snowflake.
//! - `rank.assigned`: a `role.assign` whose resolved role is a rank-ladder
//!   rung (`detail` is the rank key) with a real effect (`affected >= 1`).
//!   Idempotent no-ops (`already_held`) publish nothing: no arrival happened.
//! - `member.joined`: a `guild.add_member` with a real effect (`affected >=
//!   1`). Replays of an existing member (`already_member`) publish nothing.
//!
//! Moves and DMs never publish (§13.2): `voice.move_member`,
//! `reminder.send` and `place_offer.send` rows stay in the internal audits
//! only. Every `moderation.*` verb, `settings.*`, `automations.*` and any
//! unknown action likewise produce no entry, by the store query's closed
//! filter — not by a filter that could be misconfigured.
//!
//! Bot-side suppression is the `privacy_opt_outs` backstop: a membership
//! subject present there is emitted without `subject_id` while the entry
//! still counts. The site applies its own §4 projection (guest
//! anonymisation, `show_in_bot_log`) at serialization.
//!
//! Pagination is strictly ascending over an opaque `seq` cursor encoding
//! `(created_at, audit_id)`. The site stores `next` and resumes; a replayed
//! pull dedupes on `seq`.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

use super::internal_actions::{valid_event_key, ActionError, ErrorCode};

/// The verb this module serves.
pub const BOTLOG_READ_ACTION: &str = "botlog.read";
/// Env-only capability gate; default off like every other post-launch verb.
pub const BOTLOG_READ_FLAG: &str = "TWO_INTERNAL_ALLOW_BOTLOG_READ";
/// Result schema version served on the wire.
pub const BOTLOG_RESULT_VERSION: i64 = 1;
/// Page ceiling from the contract; the only bound the site may request.
pub const BOTLOG_PAGE_LIMIT_MAX: i64 = 200;
/// Limit when the request names none.
pub const BOTLOG_PAGE_LIMIT_DEFAULT: i64 = 200;
/// Two non-negative i64 values and one separator: 39 raw bytes, base64url.
pub const BOTLOG_CURSOR_MAX_BYTES: usize = 52;

/// One publishable log line, in contract §6 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotlogKind {
    EventOpened,
    EventUpdated,
    EventCancelled,
    AnnouncementPosted,
    RankAssigned,
    MemberJoined,
}

impl BotlogKind {
    /// Wire name; matches the contract's closed kind list exactly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BotlogKind::EventOpened => "event.opened",
            BotlogKind::EventUpdated => "event.updated",
            BotlogKind::EventCancelled => "event.cancelled",
            BotlogKind::AnnouncementPosted => "announcement.posted",
            BotlogKind::RankAssigned => "rank.assigned",
            BotlogKind::MemberJoined => "member.joined",
        }
    }
}

/// Exclusive lower bound of a page: rows strictly after
/// `(created_at, audit_id)`, ascending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotlogCursor {
    /// Whole microseconds since the Unix epoch; matches `TIMESTAMPTZ`
    /// microsecond storage exactly, so resumption never skips or repeats.
    pub after_micros: i64,
    /// Tie-breaker inside one microsecond.
    pub after_audit_id: i64,
}

impl BotlogCursor {
    /// Start of the log: sorts before every real row.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            after_micros: 0,
            after_audit_id: 0,
        }
    }

    /// Opaque site token; the site never interprets it.
    #[must_use]
    pub fn encode(self) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.after_micros, self.after_audit_id))
    }

    /// Fail closed before decoding oversized tokens. Accept only the canonical
    /// encoding of two non-negative integers, bounding empty-tail echoes too.
    pub fn decode(cursor: &str) -> Result<Self, ActionError> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let malformed = || {
            ActionError::new(
                ErrorCode::Malformed,
                "\"after\" must be an opaque botlog cursor",
                "bad_after",
            )
        };
        if cursor.len() > BOTLOG_CURSOR_MAX_BYTES {
            return Err(malformed());
        }
        let raw = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| malformed())?;
        let text = std::str::from_utf8(&raw).map_err(|_| malformed())?;
        let (micros, audit) = text.split_once(':').ok_or_else(malformed)?;
        let parsed = Self {
            after_micros: micros.parse().map_err(|_| malformed())?,
            after_audit_id: audit.parse().map_err(|_| malformed())?,
        };
        if parsed.after_micros < 0 || parsed.after_audit_id < 0 || parsed.encode() != cursor {
            return Err(malformed());
        }
        Ok(parsed)
    }
}

/// Validated `botlog.read` body: optional `after` cursor plus optional
/// `limit` (default 200). Unknown fields are ignored; the action itself is
/// matched by `authorize` before validation runs.
#[derive(Debug, Clone)]
pub struct BotlogReadRequest {
    pub after: BotlogCursor,
    /// Verbatim `after` token echoed as `next` when the page consumes no
    /// rows, so the site can always resume byte-identically.
    pub after_verbatim: Option<String>,
    pub limit: i64,
}

fn malformed_limit() -> ActionError {
    ActionError::new(
        ErrorCode::Malformed,
        "\"limit\" must be an integer 1-200",
        "bad_limit",
    )
}

/// `botlog.read` field validation: keyless like `event.read`, so no
/// idempotency key and no guild selector (one bot serves one guild).
pub fn validate_botlog_read(body: &Map<String, Value>) -> Result<BotlogReadRequest, ActionError> {
    let (after, after_verbatim) = match body.get("after") {
        None => (BotlogCursor::zero(), None),
        Some(Value::String(cursor)) => {
            if cursor.is_empty() {
                return Err(ActionError::new(
                    ErrorCode::Malformed,
                    "\"after\" must be an opaque botlog cursor",
                    "bad_after",
                ));
            }
            (BotlogCursor::decode(cursor)?, Some(cursor.clone()))
        }
        Some(_) => {
            return Err(ActionError::new(
                ErrorCode::Malformed,
                "\"after\" must be an opaque botlog cursor",
                "bad_after",
            ));
        }
    };
    let limit = match body.get("limit") {
        None => BOTLOG_PAGE_LIMIT_DEFAULT,
        Some(Value::Number(n)) => {
            let limit = n.as_i64().ok_or_else(malformed_limit)?;
            if !(1..=BOTLOG_PAGE_LIMIT_MAX).contains(&limit) {
                return Err(malformed_limit());
            }
            limit
        }
        Some(_) => return Err(malformed_limit()),
    };
    Ok(BotlogReadRequest {
        after,
        after_verbatim,
        limit,
    })
}

/// One store row inside the publishable window. Plain data: the store builds
/// it, this module classifies it, so the pure projection stays testable
/// without a database.
#[derive(Debug, Clone)]
pub struct BotlogAuditRow {
    pub audit_id: i64,
    /// Whole microseconds since the Unix epoch, matching `created_at`.
    pub created_micros: i64,
    /// Whole-second Zulu display instant (`at`).
    pub at_utc: String,
    pub action: String,
    pub target_id: Option<String>,
    pub resolved_role_id: Option<String>,
    pub resource_id: Option<String>,
    pub outcome: Option<String>,
    pub affected: Option<i64>,
    /// Reverse key lookup: `internal_event_keys` row for `resource_id`.
    pub event_key: Option<String>,
    /// Rank rung whose `role_id` matches `resolved_role_id`, if any.
    pub rank_key: Option<String>,
    /// Subject present in `privacy_opt_outs`: emit the entry without it.
    pub suppressed: bool,
}

/// One publishable entry on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct BotlogEntry {
    pub seq: String,
    pub at: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The contract §6 result shape.
#[derive(Debug, Clone, Serialize)]
pub struct BotlogPage {
    pub v: i64,
    pub entries: Vec<BotlogEntry>,
    pub next: String,
}

/// Closed per-kind scalar grammar: rank keys and channel keys travel the
/// same charset as website event keys (1–200 `[A-Za-z0-9._:-]`), validated
/// bot-side. Anything outside it fails closed (no entry) rather than
/// carrying unexpected text to a public feed.
fn valid_detail_scalar(value: &str) -> bool {
    valid_event_key(value)
}

/// Project one audit row to its publishable entry, or `None` when the row
/// cannot be named in the closed vocabulary. The store query already admits
/// only publishable (action, outcome) pairs; this is the naming layer:
/// reverse key lookups and the opt-out backstop.
#[must_use]
pub fn classify_botlog_row(
    row: &BotlogAuditRow,
    channel_by_id: &HashMap<String, String>,
) -> Option<BotlogEntry> {
    let seq = BotlogCursor {
        after_micros: row.created_micros,
        after_audit_id: row.audit_id,
    }
    .encode();
    let base = |kind: BotlogKind| BotlogEntry {
        seq,
        at: row.at_utc.clone(),
        kind: kind.as_str().to_owned(),
        subject_id: None,
        event_key: None,
        detail: None,
    };
    let mut entry = match (row.action.as_str(), row.outcome.as_deref()) {
        ("event.upsert", Some("created")) => {
            let mut entry = base(BotlogKind::EventOpened);
            entry.event_key = Some(row.event_key.clone()?);
            entry
        }
        ("event.upsert", Some("updated")) => {
            let mut entry = base(BotlogKind::EventUpdated);
            entry.event_key = Some(row.event_key.clone()?);
            entry
        }
        ("event.cancel", Some("cancelled")) => {
            let mut entry = base(BotlogKind::EventCancelled);
            entry.event_key = Some(row.event_key.clone()?);
            entry
        }
        ("announcement.post", _) => {
            let mut entry = base(BotlogKind::AnnouncementPosted);
            let channel_id = row.target_id.as_deref()?;
            let key = channel_by_id.get(channel_id)?;
            if !valid_detail_scalar(key) {
                return None;
            }
            entry.detail = Some(key.clone());
            entry
        }
        ("role.assign", _) => {
            if row.affected.unwrap_or(0) < 1 {
                return None;
            }
            let mut entry = base(BotlogKind::RankAssigned);
            let rank_key = row.rank_key.as_deref()?;
            if !valid_detail_scalar(rank_key) {
                return None;
            }
            entry.subject_id = row.target_id.clone();
            entry.detail = Some(rank_key.to_owned());
            entry
        }
        ("guild.add_member", _) => {
            if row.affected.unwrap_or(0) < 1 {
                return None;
            }
            let mut entry = base(BotlogKind::MemberJoined);
            entry.subject_id = row.target_id.clone();
            entry
        }
        _ => return None,
    };
    if entry.subject_id.is_none()
        && matches!(entry.kind.as_str(), "rank.assigned" | "member.joined")
    {
        // Named-subject kinds always carry their member: a missing target is
        // a row this projection cannot name, not an anonymous entry.
        return None;
    }
    if row.suppressed {
        entry.subject_id = None;
    }
    Some(entry)
}

/// Assemble the wire page: classify every consumed row, then resume after
/// the last consumed row — even rows that classify to nothing still advance
/// the cursor, so a page of unnameable rows cannot pin the site in place.
/// With no rows at all `next` echoes the request cursor, and the site stops
/// when a page is empty and `next` did not advance.
#[must_use]
pub fn assemble_botlog_page(
    rows: Vec<BotlogAuditRow>,
    channel_by_id: &HashMap<String, String>,
    request: &BotlogReadRequest,
) -> BotlogPage {
    let entries: Vec<BotlogEntry> = rows
        .iter()
        .filter_map(|row| classify_botlog_row(row, channel_by_id))
        .collect();
    let next = rows
        .last()
        .map(|row| {
            BotlogCursor {
                after_micros: row.created_micros,
                after_audit_id: row.audit_id,
            }
            .encode()
        })
        .or_else(|| request.after_verbatim.clone())
        .unwrap_or_else(|| BotlogCursor::zero().encode());
    BotlogPage {
        v: BOTLOG_RESULT_VERSION,
        entries,
        next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(action: &str) -> BotlogAuditRow {
        BotlogAuditRow {
            audit_id: 7,
            created_micros: 1_791_000_000_000_000,
            at_utc: "2026-11-03T19:04:11Z".to_owned(),
            action: action.to_owned(),
            target_id: None,
            resolved_role_id: None,
            resource_id: None,
            outcome: None,
            affected: None,
            event_key: None,
            rank_key: None,
            suppressed: false,
        }
    }

    fn channels() -> HashMap<String, String> {
        HashMap::from([("333333333333333333".to_owned(), "announcements".to_owned())])
    }

    #[test]
    fn cursor_round_trips_and_orders_ascending() {
        let cursor = BotlogCursor {
            after_micros: 1_791_000_000_123_456,
            after_audit_id: 42,
        };
        let encoded = cursor.encode();
        assert_eq!(BotlogCursor::decode(&encoded).unwrap(), cursor);
        assert!(BotlogCursor::zero().encode() != encoded);
        // Earlier instant sorts before a later one at the same audit id;
        // the tie-breaker orders same-instant rows by audit id.
        let earlier = BotlogCursor {
            after_micros: 1,
            after_audit_id: 99,
        };
        let later_same_audit = BotlogCursor {
            after_micros: 2,
            after_audit_id: 99,
        };
        assert!(
            (earlier.after_micros, earlier.after_audit_id)
                < (
                    later_same_audit.after_micros,
                    later_same_audit.after_audit_id
                )
        );
    }

    #[test]
    fn cursor_rejects_foreign_tokens() {
        for bad in [
            "",
            "not-base64!!",
            "cursor-0000",
            // A truncated prefix of a valid token: the zero cursor encodes
            // to exactly four chars, so the two-char slice is genuinely
            // truncated (it decodes to "0" with no cursor separator).
            &BotlogCursor::zero().encode()[..2],
        ] {
            assert!(
                BotlogCursor::decode(bad).is_err(),
                "{bad:?} must fail closed"
            );
        }
        // Well-formed base64 outside the two-integer grammar still refuses.
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let outside = URL_SAFE_NO_PAD.encode("1:2:3");
        assert!(BotlogCursor::decode(&outside).is_err());
        let text = URL_SAFE_NO_PAD.encode("soon:late");
        assert!(BotlogCursor::decode(&text).is_err());
    }

    #[test]
    fn cursor_bounds_and_canonical_grammar() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

        // Two non-negative i64 values need at most 39 decoded bytes or 52
        // base64url bytes. Check the boundary without a large allocation.
        let largest = BotlogCursor {
            after_micros: i64::MAX,
            after_audit_id: i64::MAX,
        };
        let encoded = largest.encode();
        assert_eq!(encoded.len(), 52);
        assert_eq!(BotlogCursor::decode(&encoded).unwrap(), largest);
        assert!(BotlogCursor::decode(&"A".repeat(53)).is_err());
        let padded = URL_SAFE_NO_PAD.encode(format!("{}:1", "0".repeat(39)));
        assert!(padded.len() > BOTLOG_CURSOR_MAX_BYTES);
        assert!(BotlogCursor::decode(&padded).is_err());
        for text in ["01:2", "+1:2", "-1:2", "1:-2", "1:02", "1:+2"] {
            let after = URL_SAFE_NO_PAD.encode(text);
            let body = serde_json::from_value(serde_json::json!({"after": after})).unwrap();
            assert!(validate_botlog_read(&body).is_err());
        }
        let request = validate_botlog_read(
            &serde_json::from_value(serde_json::json!({"after": encoded})).unwrap(),
        )
        .unwrap();
        let page = assemble_botlog_page(vec![], &channels(), &request);
        assert_eq!(page.next, encoded);
        assert_eq!(page.next.len(), 52);
    }

    #[test]
    fn validator_defaults_and_bounds() {
        let body: Map<String, Value> = Map::new();
        let request = validate_botlog_read(&body).unwrap();
        assert_eq!(request.after, BotlogCursor::zero());
        assert_eq!(request.limit, BOTLOG_PAGE_LIMIT_DEFAULT);
        assert_eq!(request.after_verbatim, None);

        let body: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "action": "botlog.read",
            "after": BotlogCursor { after_micros: 5, after_audit_id: 6 }.encode(),
            "limit": 10,
            "unknown_future_field": true,
        }))
        .unwrap();
        let request = validate_botlog_read(&body).unwrap();
        assert_eq!(request.limit, 10);
        assert!(request.after_verbatim.is_some());

        for bad_limit in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(201),
            serde_json::json!(1.5),
            serde_json::json!("200"),
            serde_json::json!(null),
        ] {
            let body: Map<String, Value> = serde_json::from_value(serde_json::json!({
                "limit": bad_limit,
            }))
            .unwrap();
            assert!(
                validate_botlog_read(&body).is_err(),
                "{bad_limit} must fail closed"
            );
        }
        let body: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "after": "cursor-0000",
        }))
        .unwrap();
        assert!(validate_botlog_read(&body).is_err());
    }

    #[test]
    fn event_kinds_name_the_key_not_the_actor() {
        let mut upsert = row("event.upsert");
        upsert.outcome = Some("created".to_owned());
        upsert.event_key = Some("raid-night-2026-11-07".to_owned());
        upsert.target_id = Some("234567890123456789".to_owned());
        let entry = classify_botlog_row(&upsert, &channels()).unwrap();
        assert_eq!(entry.kind, "event.opened");
        assert_eq!(entry.event_key.as_deref(), Some("raid-night-2026-11-07"));
        assert_eq!(entry.subject_id, None);
        assert_eq!(entry.detail, None);

        let mut cancel = row("event.cancel");
        cancel.outcome = Some("cancelled".to_owned());
        cancel.event_key = Some("raid-night-2026-11-07".to_owned());
        let entry = classify_botlog_row(&cancel, &channels()).unwrap();
        assert_eq!(entry.kind, "event.cancelled");

        // An outcome the projection does not name never publishes.
        let mut other = row("event.upsert");
        other.outcome = Some("reconciled".to_owned());
        other.event_key = Some("raid-night-2026-11-07".to_owned());
        assert!(classify_botlog_row(&other, &channels()).is_none());

        // A success the reverse lookup cannot name fails closed.
        let mut unmapped = row("event.cancel");
        unmapped.outcome = Some("cancelled".to_owned());
        assert!(classify_botlog_row(&unmapped, &channels()).is_none());
    }

    #[test]
    fn announcement_names_the_channel_key_only() {
        let mut posted = row("announcement.post");
        posted.target_id = Some("333333333333333333".to_owned());
        let entry = classify_botlog_row(&posted, &channels()).unwrap();
        assert_eq!(entry.kind, "announcement.posted");
        assert_eq!(entry.detail.as_deref(), Some("announcements"));
        assert_eq!(entry.subject_id, None);
        assert_eq!(entry.event_key, None);

        // A channel that no longer maps to a configured key publishes
        // nothing rather than leaking a raw snowflake.
        let mut drifted = row("announcement.post");
        drifted.target_id = Some("999999999999999999".to_owned());
        assert!(classify_botlog_row(&drifted, &channels()).is_none());
    }

    #[test]
    fn membership_needs_a_real_effect_and_a_nameable_subject() {
        let mut assigned = row("role.assign");
        assigned.target_id = Some("100000000000000001".to_owned());
        assigned.resolved_role_id = Some("555555555555555555".to_owned());
        assigned.affected = Some(1);
        assigned.rank_key = Some("ember-3".to_owned());
        let entry = classify_botlog_row(&assigned, &channels()).unwrap();
        assert_eq!(entry.kind, "rank.assigned");
        assert_eq!(entry.subject_id.as_deref(), Some("100000000000000001"));
        assert_eq!(entry.detail.as_deref(), Some("ember-3"));

        // Non-rank grants, no-op replays and nameless rows never publish.
        let mut plain_grant = assigned.clone();
        plain_grant.rank_key = None;
        assert!(classify_botlog_row(&plain_grant, &channels()).is_none());
        let mut no_op = assigned.clone();
        no_op.affected = Some(0);
        assert!(classify_botlog_row(&no_op, &channels()).is_none());
        let mut nameless = assigned.clone();
        nameless.target_id = None;
        assert!(classify_botlog_row(&nameless, &channels()).is_none());

        let mut joined = row("guild.add_member");
        joined.target_id = Some("100000000000000002".to_owned());
        joined.affected = Some(1);
        let entry = classify_botlog_row(&joined, &channels()).unwrap();
        assert_eq!(entry.kind, "member.joined");
        assert_eq!(entry.subject_id.as_deref(), Some("100000000000000002"));

        let mut rejoin = joined.clone();
        rejoin.affected = Some(0);
        assert!(classify_botlog_row(&rejoin, &channels()).is_none());
    }

    #[test]
    fn moderation_moves_failures_and_unknowns_never_publish() {
        for action in [
            "moderation.ban",
            "moderation.timeout",
            "settings.set",
            "automations.export",
            "voice.move_member",
            "reminder.send",
            "place_offer.send",
            "voice.snapshot",
            "event.read",
            "role.assign",
        ] {
            let mut row = row(action);
            row.target_id = Some("100000000000000001".to_owned());
            row.affected = Some(1);
            // Even a terminal success for a non-listed verb publishes
            // nothing: moves and DMs stay in the internal audits only.
            assert!(
                classify_botlog_row(&row, &channels()).is_none(),
                "{action} must never publish"
            );
        }
    }

    #[test]
    fn suppression_omits_the_subject_and_keeps_the_entry() {
        let mut assigned = row("role.assign");
        assigned.target_id = Some("100000000000000001".to_owned());
        assigned.affected = Some(1);
        assigned.rank_key = Some("ember-3".to_owned());
        assigned.suppressed = true;
        let entry = classify_botlog_row(&assigned, &channels()).unwrap();
        assert_eq!(entry.kind, "rank.assigned");
        assert_eq!(entry.subject_id, None);
        assert_eq!(entry.detail.as_deref(), Some("ember-3"));
    }

    #[test]
    fn page_advances_past_unnameable_rows_and_echoes_at_the_tail() {
        let request = BotlogReadRequest {
            after: BotlogCursor::zero(),
            after_verbatim: None,
            limit: 200,
        };
        let mut unmapped = row("event.cancel");
        unmapped.audit_id = 8;
        unmapped.created_micros = 2_000_000_000_000_000;
        unmapped.outcome = Some("cancelled".to_owned());
        let page = assemble_botlog_page(vec![unmapped], &channels(), &request);
        assert!(page.entries.is_empty());
        // Consumed but unnameable: `next` advanced past the row so the site
        // cannot pin on it.
        assert_eq!(
            page.next,
            BotlogCursor {
                after_micros: 2_000_000_000_000_000,
                after_audit_id: 8,
            }
            .encode()
        );

        let tail = assemble_botlog_page(vec![], &channels(), &request);
        assert!(tail.entries.is_empty());
        assert_eq!(tail.next, BotlogCursor::zero().encode());

        let resumed = BotlogReadRequest {
            after: BotlogCursor {
                after_micros: 1,
                after_audit_id: 2,
            },
            after_verbatim: Some("verbatim-after".to_owned()),
            limit: 200,
        };
        let tail = assemble_botlog_page(vec![], &channels(), &resumed);
        assert_eq!(tail.next, "verbatim-after");
        assert_eq!(page.v, BOTLOG_RESULT_VERSION);
    }
}
