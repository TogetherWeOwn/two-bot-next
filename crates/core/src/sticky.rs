//! Sticky messages: `/sticky`, `/sticky-remove` + debounced re-post.
//!
//! Slice TOG-10082 of TOG-9809 S4. Ports the per-channel sticky behaviour
//! from legacy two-bot as framework-free domain logic (validate → decide →
//! plain-data outcomes, same style as `leveling.rs`/`moderation.rs`) plus a
//! `#[cfg(feature = "db")]` sqlx store. The S4 interaction router (TOG-10075)
//! registers the `/sticky` / `/sticky-remove` handlers and the S4 REST
//! executor (TOG-10076) performs the Discord side effects; until both land,
//! callers drive the validators + store primitives below directly — no
//! private dispatcher or HTTP client lives here.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - shapes: `src/automations/discord.ts` (`sticky` body req + `debounce`
//!   1–300 opt, `sticky-remove` bare; both `ManageGuild`, current channel) —
//!   already ported in `feature_commands.rs`, referenced here, not redefined.
//! - service: `src/automations/service.ts` (`putSticky`, `deleteSticky`,
//!   `onChannelActivity` + `AutomationDiscord::{postMessage, deleteMessage}`).
//! - store: `src/automations/store.ts` (`getSticky`, `putSticky`,
//!   `claimStickyPost`, `recordStickyPost`, `releaseStickyPost`,
//!   `deleteSticky`, `audit`).
//! - gateway: `src/automations/gateway.ts` (`automationMessageAccepted`
//!   sticky half: guild match, `author.bot` guard, metadata-only re-post
//!   after automod accepts the message).
//! - DDL: `migrations/0015_automations.sql` (`sticky_messages`,
//!   `automation_audit_log`) + `0016_automation_claims.sql` (claim columns).
//!   Ported as `crates/cutover/migrations/0150_sticky_messages.sql` (this
//!   card's reserved block 0150–0159); timestamps become `timestamptz`
//!   (same conversion the funnel port applies), names and checks preserved.
//!
//! Discord order (legacy `onChannelActivity`): claim → post replacement →
//! record → delete previous (best-effort, errors swallowed) → audit. Posting
//! before deleting avoids losing the sticky when the post fails; a failed
//! post releases the claim and deletes any orphan replacement. The card
//! acceptance phrase "previous sticky deleted before re-post" is the visible
//! end state (old gone, fresh copy at the bottom), not the wire order —
//! delete-then-post would drop the sticky entirely whenever the post fails.
//!
//! Burst coalescing is the atomic claim, not the pure check: concurrent
//! activity passes through the store's `claim_sticky_post` (one `UPDATE …
//! RETURNING`), so exactly one attempt wins the re-post window while the
//! losers report [`ActivityOutcome::Held`]. The pure
//! [`decide_activity`] mirrors the predicate for tests and pre-checks.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Constants (legacy `service.ts` + `0015_automations.sql`)
// ---------------------------------------------------------------------------

/// Default quiet window before a re-post (legacy `debounceSeconds ?? 5`).
pub const DEFAULT_DEBOUNCE_SECONDS: u64 = 5;
/// Debounce floor (legacy `debounce < 1` rejected + `setMinValue(1)`).
pub const MIN_DEBOUNCE_SECONDS: u64 = 1;
/// Debounce ceiling (legacy `debounce > 300` rejected + `setMaxValue(300)`).
pub const MAX_DEBOUNCE_SECONDS: u64 = 300;
/// Sticky body ceiling in characters (legacy `MAX_BODY = 2000`; the DB
/// `CHECK (length(body) BETWEEN 1 AND 2000)` agrees. Postgres `length()`
/// counts characters while JS `.length` counts UTF-16 units — identical for
/// BMP text, one-per-emoji apart for astral-plane text; the Rust side counts
/// `char`s, matching Postgres).
pub const MAX_BODY_CHARS: usize = 2000;
/// Stale-claim horizon (legacy `expiredClaimCutoff = now - 60_000`): a claim
/// older than this no longer blocks a new attempt (crashed re-poster).
pub const CLAIM_EXPIRY_SECONDS: u64 = 60;

// ---------------------------------------------------------------------------
// Validation (legacy `putSticky`: `requireBody` + debounce range)
// ---------------------------------------------------------------------------

/// Invalid `/sticky` input (legacy `Error('Sticky body must be …')` /
/// `Error('Debounce must be between 1 and 300 seconds.')`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StickyError {
    #[error("Sticky body must be between 1 and 2000 characters.")]
    BodyLength,
    #[error("Debounce must be between 1 and 300 seconds.")]
    BadDebounce,
}

/// Validate a sticky body: 1–2000 characters (legacy `requireBody`, no trim —
/// whitespace-only bodies pass, matching legacy).
pub fn validate_body(body: &str) -> Result<(), StickyError> {
    let len = body.chars().count();
    if !(1..=MAX_BODY_CHARS).contains(&len) {
        return Err(StickyError::BodyLength);
    }
    Ok(())
}

/// Resolve the `debounce` option: `None` (omitted) → default 5; `Some(v)`
/// must land in 1–300 (legacy `input.debounceSeconds ?? 5` + range check).
/// The Discord option already clamps 1–300; the service re-checks.
pub fn normalize_debounce(value: Option<i64>) -> Result<u64, StickyError> {
    match value {
        None => Ok(DEFAULT_DEBOUNCE_SECONDS),
        Some(v) => {
            if v < MIN_DEBOUNCE_SECONDS as i64 || v > MAX_DEBOUNCE_SECONDS as i64 {
                return Err(StickyError::BadDebounce);
            }
            Ok(v as u64)
        }
    }
}

// ---------------------------------------------------------------------------
// Reply text (legacy `discord.ts` ephemeral confirmations)
// ---------------------------------------------------------------------------

/// `/sticky` confirmation (legacy `` `Sticky set for <#${channelId}>` `` plus
/// `` `, ${debounce}s debounce` `` only when the option was passed).
#[must_use]
pub fn sticky_set_reply(channel_id: &str, debounce: Option<u64>) -> String {
    match debounce {
        Some(d) => format!("Sticky set for <#{channel_id}>, {d}s debounce."),
        None => format!("Sticky set for <#{channel_id}>."),
    }
}

/// `/sticky-remove` confirmation (legacy `'Sticky removed.'` /
/// `'No sticky in this channel.'`).
#[must_use]
pub fn sticky_removed_reply(removed: bool) -> &'static str {
    if removed {
        "Sticky removed."
    } else {
        "No sticky in this channel."
    }
}

// ---------------------------------------------------------------------------
// State + decisions (framework-free projection of the sticky row)
// ---------------------------------------------------------------------------

/// Framework-free projection of one `sticky_messages` row: everything the
/// debounce decision needs, no sqlx/time/Discord types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyState {
    pub guild_id: String,
    pub channel_id: String,
    pub body: String,
    pub debounce_seconds: u64,
    pub enabled: bool,
    pub last_message_id: Option<String>,
    /// Milliseconds since the Unix epoch (`last_posted_at`); `None` = never
    /// posted. `i64` so clock skew (`now < last`) is representable.
    pub last_posted_at_ms: Option<i64>,
}

/// True when the debounce window has elapsed: never posted, or
/// `now - last >= debounce * 1000` (legacy `last_posted_at <= cutoff` —
/// the exact boundary posts). Clock skew (`now < last`) holds.
#[must_use]
pub fn repost_due(last_posted_at_ms: Option<i64>, now_ms: i64, debounce_seconds: u64) -> bool {
    let Some(last) = last_posted_at_ms else {
        return true;
    };
    let elapsed = now_ms.saturating_sub(last);
    elapsed >= (debounce_seconds.saturating_mul(1000)) as i64
}

/// True when an existing claim still blocks a new attempt: claimed and
/// `now - claimed < 60s` (legacy `claim_token IS NULL OR claimed_at <=
/// expiredCutoff` inverted — at exactly 60s the claim is stale).
/// A future-dated claim (skew) conservatively blocks.
#[must_use]
pub fn claim_blocks(claimed_at_ms: Option<i64>, now_ms: i64) -> bool {
    let Some(claimed) = claimed_at_ms else {
        return false;
    };
    now_ms.saturating_sub(claimed) < (CLAIM_EXPIRY_SECONDS.saturating_mul(1000)) as i64
}

/// Whether gateway activity reaches the sticky check at all (legacy
/// `registerAutomationGateway`: wrong/absent guild, absent channel, or a bot
/// author — including our own re-posts — never retrigger).
#[must_use]
pub fn activity_eligible(author_is_bot: bool, guild_matches: bool, channel_present: bool) -> bool {
    !author_is_bot && guild_matches && channel_present
}

/// Pure decision for one accepted message, mirroring the store claim
/// predicate for tests and pre-checks. The atomic claim stays authoritative
/// under concurrency: two racing activities can both read `Repost` here while
/// only one wins the `UPDATE … RETURNING`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityDecision {
    /// Never reaches the check (bot author, foreign guild, no channel).
    Ignore,
    /// No enabled sticky in this channel.
    NoSticky,
    /// Inside the debounce window (or claim held): coalesced, no re-post.
    Hold,
    /// Outside the window: post `body`, then delete `previous_message_id`
    /// (best-effort) once the replacement is recorded.
    Repost {
        body: String,
        previous_message_id: Option<String>,
    },
}

/// Decide one activity against an optional sticky row.
#[must_use]
pub fn decide_activity(
    state: Option<&StickyState>,
    author_is_bot: bool,
    guild_matches: bool,
    channel_present: bool,
    now_ms: i64,
) -> ActivityDecision {
    if !activity_eligible(author_is_bot, guild_matches, channel_present) {
        return ActivityDecision::Ignore;
    }
    let Some(row) = state else {
        return ActivityDecision::NoSticky;
    };
    if !row.enabled {
        return ActivityDecision::NoSticky;
    }
    if !repost_due(row.last_posted_at_ms, now_ms, row.debounce_seconds) {
        return ActivityDecision::Hold;
    }
    ActivityDecision::Repost {
        body: row.body.clone(),
        previous_message_id: row.last_message_id.clone(),
    }
}

// ---------------------------------------------------------------------------
// Service-level outcomes (legacy `onChannelActivity` / `deleteSticky`
// results — the strings the router/executor will map)
// ---------------------------------------------------------------------------

/// Result of one `onChannelActivity` pass (legacy `'reposted' | 'held' |
/// 'none'`). `Held` covers both debounce-hold and lost-claim races; neither
/// writes an audit row in legacy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityOutcome {
    Reposted,
    Held,
    None,
}

impl ActivityOutcome {
    /// Legacy result string (`'reposted' | 'held' | 'none'`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reposted => "reposted",
            Self::Held => "held",
            Self::None => "none",
        }
    }
}

/// Result of a `/sticky-remove` (legacy `deleteSticky` boolean + the deleted
/// row's `lastMessageId` for Discord cleanup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// Row deleted; the executor best-effort deletes the Discord message.
    Removed { previous_message_id: Option<String> },
    /// No row existed (`'absent'` audit outcome).
    Absent,
}

/// The single winner's post plan from an atomic claim: the body to post plus
/// the previous message id to delete (best-effort) once the replacement is
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimGrant {
    pub body: String,
    pub previous_message_id: Option<String>,
}

/// Audit actions for `automation_audit_log` (legacy `sticky.create` /
/// `sticky.update` / `sticky.delete` / `sticky.run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyAuditAction {
    Create,
    Update,
    Delete,
    Run,
}

impl StickyAuditAction {
    /// Legacy action string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "sticky.create",
            Self::Update => "sticky.update",
            Self::Delete => "sticky.delete",
            Self::Run => "sticky.run",
        }
    }
}

/// Audit outcomes (legacy `'ok'` / `'rejected'` / `'absent'` /
/// `'post_failed'`; holds write no row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyAuditOutcome {
    Ok,
    Rejected,
    Absent,
    PostFailed,
}

/// Validated inputs for [`store::put_sticky`] (legacy `PutStickyInput`).
/// The caller validates `body` via [`validate_body`] and `debounce` via
/// [`normalize_debounce`] first; the store never validates. `/sticky` passes
/// `enabled: true` (legacy `input.enabled ?? true`); the flag exists so an
/// import path can stage a disabled row without a second write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutSticky<'a> {
    pub guild_id: &'a str,
    pub channel_id: &'a str,
    pub body: &'a str,
    pub debounce_seconds: u64,
    pub enabled: bool,
    pub actor_id: &'a str,
    pub now_ms: i64,
}

/// One automation audit row for the `sticky.*` actions (legacy
/// `AutomationAuditInput`). Holds ids and outcomes only — never member or
/// message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyAudit<'a> {
    pub guild_id: &'a str,
    /// `None` for system actors (the sticky re-poster).
    pub actor_id: Option<&'a str>,
    pub action: StickyAuditAction,
    pub target_key: Option<&'a str>,
    pub outcome: StickyAuditOutcome,
    pub reason: Option<&'a str>,
    pub at_ms: i64,
}

impl StickyAuditOutcome {
    /// Legacy outcome string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "rejected",
            Self::Absent => "absent",
            Self::PostFailed => "post_failed",
        }
    }
}

// ---------------------------------------------------------------------------
// Env gates (legacy `src/automations/config.ts` — shared with the whole
// automation slice; re-exported shape so the router reads one place)
// ---------------------------------------------------------------------------

/// Read the automation env gates from an explicit map (tests, staged config).
/// `TWO_AUTOMATIONS=1` publishes + serves `/sticky` / `/sticky-remove`
/// (same gate as the sibling automation slices); `TWO_TEXT_COMMANDS` does not
/// affect stickies (metadata-only, no MessageContent needed).
#[must_use]
pub fn automations_enabled(vars: &HashMap<String, String>) -> bool {
    vars.get("TWO_AUTOMATIONS").is_some_and(|v| v == "1")
}

// ---------------------------------------------------------------------------
// Store (`store.ts` port — one statement per method, validation lives above)
// ---------------------------------------------------------------------------

/// sqlx store for sticky state. Timestamps cross the boundary as millisecond
/// Unix times (`i64`); SQL converts via `to_timestamp(ms / 1000.0)` and reads
/// back via `(extract(epoch from …) * 1000)::bigint`, so this module needs no
/// time crate — matching the cutover store's string-free numeric style.
#[cfg(feature = "db")]
pub mod store {
    use sqlx::{Pool, Postgres};

    use super::{PutSticky, StickyAudit, StickyState};

    /// One decoded `sticky_messages` row (column order of `get_sticky`).
    type StickyRow = (
        String,
        String,
        String,
        i32,
        bool,
        Option<String>,
        Option<i64>,
    );

    /// Fetch one channel's sticky (`store.getSticky`).
    pub async fn get_sticky(
        pool: &Pool<Postgres>,
        guild_id: &str,
        channel_id: &str,
    ) -> Result<Option<StickyState>, sqlx::Error> {
        let row: Option<StickyRow> = sqlx::query_as(
            "SELECT guild_id, channel_id, body, debounce_seconds, enabled, last_message_id,
                        (extract(epoch from last_posted_at) * 1000)::BIGINT AS last_posted_at_ms
                   FROM sticky_messages WHERE guild_id = $1 AND channel_id = $2",
        )
        .bind(guild_id)
        .bind(channel_id)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(
            |(
                guild_id,
                channel_id,
                body,
                debounce,
                enabled,
                last_message_id,
                last_posted_at_ms,
            )| {
                StickyState {
                    guild_id,
                    channel_id,
                    body,
                    debounce_seconds: debounce.max(0) as u64,
                    enabled,
                    last_message_id,
                    last_posted_at_ms,
                }
            },
        ))
    }

    /// Upsert a sticky (`store.putSticky`): insert carries NULL post columns;
    /// on conflict the post/creation columns are preserved and any claim is
    /// cleared. Returns `true` when the row was created (`xmax = 0`
    /// distinguishes insert from update atomically — no read-then-write
    /// race; legacy read first only to name the audit action).
    pub async fn put_sticky(
        pool: &Pool<Postgres>,
        input: &PutSticky<'_>,
    ) -> Result<bool, sqlx::Error> {
        let created: (bool,) = sqlx::query_as(
            "INSERT INTO sticky_messages
               (guild_id, channel_id, body, debounce_seconds, enabled,
                created_by, created_at, updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6,
                     to_timestamp($7::DOUBLE PRECISION / 1000.0),
                     $8,
                     to_timestamp($9::DOUBLE PRECISION / 1000.0))
             ON CONFLICT (guild_id, channel_id) DO UPDATE SET
               body             = excluded.body,
               debounce_seconds = excluded.debounce_seconds,
               enabled          = excluded.enabled,
               updated_by       = excluded.updated_by,
               updated_at       = excluded.updated_at,
               claim_token      = NULL,
               claimed_at       = NULL
             RETURNING (xmax = 0) AS created",
        )
        .bind(input.guild_id)
        .bind(input.channel_id)
        .bind(input.body)
        .bind(input.debounce_seconds as i32)
        .bind(input.enabled)
        .bind(input.actor_id)
        .bind(input.now_ms)
        .bind(input.actor_id)
        .bind(input.now_ms)
        .fetch_one(pool)
        .await?;
        Ok(created.0)
    }

    /// Atomically claim one re-post window (`store.claimStickyPost`). The
    /// timestamp update is the lock: debounce is read from the row itself
    /// (`last_posted_at <= now - debounce_seconds * interval '1 second'`),
    /// so a concurrent debounce edit cannot slip between a read and the
    /// claim. Returns the post plan for the single winner; `None` means held
    /// (disabled, debounced, or claimed).
    pub async fn claim_sticky_post(
        pool: &Pool<Postgres>,
        guild_id: &str,
        channel_id: &str,
        claim_token: &str,
        now_ms: i64,
    ) -> Result<Option<super::ClaimGrant>, sqlx::Error> {
        let expired_cutoff_ms =
            now_ms.saturating_sub((super::CLAIM_EXPIRY_SECONDS.saturating_mul(1000)) as i64);
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "UPDATE sticky_messages
                SET claim_token = $3,
                    claimed_at = to_timestamp($4::DOUBLE PRECISION / 1000.0)
              WHERE guild_id = $1 AND channel_id = $2 AND enabled = TRUE
                AND (claim_token IS NULL
                     OR claimed_at <= to_timestamp($5::DOUBLE PRECISION / 1000.0))
                AND (last_posted_at IS NULL
                     OR last_posted_at <= to_timestamp($4::DOUBLE PRECISION / 1000.0)
                        - (debounce_seconds * interval '1 second'))
              RETURNING body, last_message_id",
        )
        .bind(guild_id)
        .bind(channel_id)
        .bind(claim_token)
        .bind(now_ms)
        .bind(expired_cutoff_ms)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|(body, previous_message_id)| super::ClaimGrant {
            body,
            previous_message_id,
        }))
    }

    /// Record a successful replacement post (`store.recordStickyPost`):
    /// remembers the new message id, stamps the post time, releases the
    /// claim. Returns `false` when the claim moved on (caller must delete
    /// the orphan replacement — legacy does exactly this).
    pub async fn record_sticky_post(
        pool: &Pool<Postgres>,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
        posted_at_ms: i64,
        claim_token: &str,
    ) -> Result<bool, sqlx::Error> {
        let done = sqlx::query(
            "UPDATE sticky_messages SET
               last_message_id = $3,
               last_posted_at = to_timestamp($4::DOUBLE PRECISION / 1000.0),
               claim_token = NULL, claimed_at = NULL
              WHERE guild_id = $1 AND channel_id = $2 AND claim_token = $5",
        )
        .bind(guild_id)
        .bind(channel_id)
        .bind(message_id)
        .bind(posted_at_ms)
        .bind(claim_token)
        .execute(pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Release only the claim this attempt acquired (`store.releaseStickyPost`,
    /// post-failure path — preserves a newer claim).
    pub async fn release_sticky_post(
        pool: &Pool<Postgres>,
        guild_id: &str,
        channel_id: &str,
        claim_token: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE sticky_messages SET claim_token = NULL, claimed_at = NULL
              WHERE guild_id = $1 AND channel_id = $2 AND claim_token = $3",
        )
        .bind(guild_id)
        .bind(channel_id)
        .bind(claim_token)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Atomically delete a sticky (`store.deleteSticky` + `deleteSticky`
    /// service half: legacy deletes the message best-effort after the row
    /// delete). [`RemoveOutcome::Removed`] carries the previous message id
    /// for Discord cleanup; [`RemoveOutcome::Absent`] means no row existed.
    pub async fn delete_sticky(
        pool: &Pool<Postgres>,
        guild_id: &str,
        channel_id: &str,
    ) -> Result<super::RemoveOutcome, sqlx::Error> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "DELETE FROM sticky_messages
              WHERE guild_id = $1 AND channel_id = $2
              RETURNING last_message_id",
        )
        .bind(guild_id)
        .bind(channel_id)
        .fetch_optional(pool)
        .await?;
        Ok(match row {
            Some((previous_message_id,)) => super::RemoveOutcome::Removed {
                previous_message_id,
            },
            None => super::RemoveOutcome::Absent,
        })
    }

    /// Append an automation audit row (`store.audit` for the `sticky.*`
    /// actions). The id is DB-generated (`md5(random() …)`) so this module
    /// needs no uuid crate; legacy `randomUUID()` and this are both opaque
    /// unique text. Holds no member content — ids and outcomes only.
    pub async fn audit_sticky(
        pool: &Pool<Postgres>,
        row: &StickyAudit<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO automation_audit_log
               (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
             VALUES (md5(random()::text || clock_timestamp()::text),
                     $1, $2, $3, $4, $5, $6,
                     to_timestamp($7::DOUBLE PRECISION / 1000.0))",
        )
        .bind(row.guild_id)
        .bind(row.actor_id)
        .bind(row.action.as_str())
        .bind(row.target_key)
        .bind(row.outcome.as_str())
        .bind(row.reason)
        .bind(row.at_ms)
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn state() -> StickyState {
        StickyState {
            guild_id: "g1".to_owned(),
            channel_id: "c1".to_owned(),
            body: "pinned info".to_owned(),
            debounce_seconds: 5,
            enabled: true,
            last_message_id: Some("m-old".to_owned()),
            last_posted_at_ms: Some(NOW - 60_000),
        }
    }

    #[test]
    fn body_length_bounds_match_legacy() {
        assert_eq!(validate_body(""), Err(StickyError::BodyLength));
        assert!(validate_body("x").is_ok());
        assert!(validate_body(&"x".repeat(2000)).is_ok());
        assert_eq!(
            validate_body(&"x".repeat(2001)),
            Err(StickyError::BodyLength)
        );
        // Multibyte counts as characters (matches Postgres length()).
        assert!(validate_body(&"é".repeat(2000)).is_ok());
    }

    #[test]
    fn debounce_defaults_to_five_and_clamps_1_to_300() {
        assert_eq!(normalize_debounce(None), Ok(5));
        assert_eq!(normalize_debounce(Some(1)), Ok(1));
        assert_eq!(normalize_debounce(Some(300)), Ok(300));
        assert_eq!(normalize_debounce(Some(0)), Err(StickyError::BadDebounce));
        assert_eq!(normalize_debounce(Some(301)), Err(StickyError::BadDebounce));
        assert_eq!(normalize_debounce(Some(-1)), Err(StickyError::BadDebounce));
    }

    #[test]
    fn repost_due_needs_a_full_quiet_window() {
        // Never posted → due.
        assert!(repost_due(None, NOW, 5));
        // 4s after a 5s-debounce post → held (bursts coalesce).
        assert!(!repost_due(Some(NOW - 4_000), NOW, 5));
        // Exact boundary posts (legacy `<= cutoff`).
        assert!(repost_due(Some(NOW - 5_000), NOW, 5));
        assert!(repost_due(Some(NOW - 6_000), NOW, 5));
        // Clock skew (now behind last) holds rather than re-posting.
        assert!(!repost_due(Some(NOW + 10_000), NOW, 5));
    }

    #[test]
    fn stale_claims_stop_blocking_at_sixty_seconds() {
        assert!(!claim_blocks(None, NOW));
        assert!(claim_blocks(Some(NOW - 59_999), NOW));
        // Exactly 60s → stale (legacy `claimed_at <= expiredCutoff`).
        assert!(!claim_blocks(Some(NOW - 60_000), NOW));
        assert!(!claim_blocks(Some(NOW - 120_000), NOW));
        // Future-dated claim blocks (conservative on skew).
        assert!(claim_blocks(Some(NOW + 5_000), NOW));
    }

    #[test]
    fn bots_never_retrigger() {
        assert!(!activity_eligible(true, true, true));
        assert!(!activity_eligible(false, false, true));
        assert!(!activity_eligible(false, true, false));
        assert!(activity_eligible(false, true, true));
    }

    #[test]
    fn decide_activity_covers_the_gateway_matrix() {
        // Bot's own re-post never retriggers.
        assert_eq!(
            decide_activity(Some(&state()), true, true, true, NOW),
            ActivityDecision::Ignore
        );
        // Foreign guild / missing channel never reach the check.
        assert_eq!(
            decide_activity(Some(&state()), false, false, true, NOW),
            ActivityDecision::Ignore
        );
        assert_eq!(
            decide_activity(Some(&state()), false, true, false, NOW),
            ActivityDecision::Ignore
        );
        // No row, or a disabled row, reports none (never held).
        assert_eq!(
            decide_activity(None, false, true, true, NOW),
            ActivityDecision::NoSticky
        );
        let mut disabled = state();
        disabled.enabled = false;
        assert_eq!(
            decide_activity(Some(&disabled), false, true, true, NOW),
            ActivityDecision::NoSticky
        );
        // Inside the window → hold (burst coalescing).
        let mut fresh = state();
        fresh.last_posted_at_ms = Some(NOW - 1_000);
        assert_eq!(
            decide_activity(Some(&fresh), false, true, true, NOW),
            ActivityDecision::Hold
        );
        // Outside the window → repost carries body + previous id for the
        // post → record → delete-previous order.
        assert_eq!(
            decide_activity(Some(&state()), false, true, true, NOW),
            ActivityDecision::Repost {
                body: "pinned info".to_owned(),
                previous_message_id: Some("m-old".to_owned()),
            }
        );
        // First post after set (never posted) reposts with nothing to delete.
        let mut never = state();
        never.last_posted_at_ms = None;
        never.last_message_id = None;
        assert_eq!(
            decide_activity(Some(&never), false, true, true, NOW),
            ActivityDecision::Repost {
                body: "pinned info".to_owned(),
                previous_message_id: None,
            }
        );
    }

    #[test]
    fn reply_text_matches_legacy() {
        assert_eq!(
            sticky_set_reply("c1", Some(10)),
            "Sticky set for <#c1>, 10s debounce."
        );
        // Omitted debounce carries no suffix.
        assert_eq!(sticky_set_reply("c1", None), "Sticky set for <#c1>.");
        assert_eq!(sticky_removed_reply(true), "Sticky removed.");
        assert_eq!(sticky_removed_reply(false), "No sticky in this channel.");
    }

    #[test]
    fn outcome_strings_match_legacy() {
        assert_eq!(ActivityOutcome::Reposted.as_str(), "reposted");
        assert_eq!(ActivityOutcome::Held.as_str(), "held");
        assert_eq!(ActivityOutcome::None.as_str(), "none");
        assert_eq!(StickyAuditAction::Create.as_str(), "sticky.create");
        assert_eq!(StickyAuditAction::Update.as_str(), "sticky.update");
        assert_eq!(StickyAuditAction::Delete.as_str(), "sticky.delete");
        assert_eq!(StickyAuditAction::Run.as_str(), "sticky.run");
        assert_eq!(StickyAuditOutcome::Ok.as_str(), "ok");
        assert_eq!(StickyAuditOutcome::Rejected.as_str(), "rejected");
        assert_eq!(StickyAuditOutcome::Absent.as_str(), "absent");
        assert_eq!(StickyAuditOutcome::PostFailed.as_str(), "post_failed");
    }

    #[test]
    fn automations_gate_is_opt_in() {
        assert!(!automations_enabled(&HashMap::new()));
        let vars: HashMap<String, String> = [("TWO_AUTOMATIONS", "1")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert!(automations_enabled(&vars));
    }
}
