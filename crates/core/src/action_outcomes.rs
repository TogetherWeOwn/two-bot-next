//! Feature action outcomes: the framework-free values the Discord REST
//! action executor turns into HTTP calls.
//!
//! Slice 4 of TOG-9809 (card TOG-10076, parity §6 Discord REST row). Every
//! feature slice produces plain-data outcomes from its domain logic (same
//! style as `leveling.rs`/`moderation.rs`); the executor in `two-bot-discord`
//! translates these values into REST calls with legacy pacing. This module
//! knows nothing about HTTP, twilight, or Discord wire types — the same
//! adjudication runs in unit tests, the interaction router, and the executor.
//!
//! Legacy sources (frozen `two-bot` `main`):
//! - `src/moderation/service.ts` (`ModerationResult`: `banned`,
//!   `temporarily_banned`, `kicked`, `timed_out`, `warned`, `purged`,
//!   `slowmode_updated`, `locked_down`, `unlocked`; plus the unban sweep).
//! - `src/discord/kick.ts` (`KickResult`: `kicked`, `already_gone`,
//!   `forbidden`, `rate_limited`, `failed` + `attempts`).
//! - `src/leveling/discord.ts` (`rankText`, `applyLevelRoles`) and
//!   `src/audit/service.ts` / `src/automations/discord.ts` /
//!   `src/announcements/discord.ts` (`allowedMentions: { parse: [] }` default,
//!   `nonce`/`enforce_nonce` on audit posts).
//!
//! Deliberately out of scope: the REST transport itself (executor slice),
//! warn persistence (warns are store-only — no Discord call, legacy
//! `carryOut` writes the ledger row and returns `warned`), and tempban expiry
//! staging (the unban sweep owns its store rows).

use super::moderation::ModerationAction;

/// What one executed feature action produced (legacy `ModerationResult`).
///
/// Channel actions record how many messages they touched; member actions
/// record whose membership changed. `Warned` carries no Discord effect —
/// legacy `carryOut` writes the ledger row and never calls Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Banned {
        user_id: String,
    },
    TemporarilyBanned {
        user_id: String,
        duration_seconds: u64,
    },
    Kicked {
        user_id: String,
    },
    TimedOut {
        user_id: String,
        duration_seconds: u64,
    },
    Warned {
        user_id: String,
    },
    Unbanned {
        user_id: String,
    },
    Purged {
        channel_id: String,
        affected: usize,
    },
    SlowmodeUpdated {
        channel_id: String,
        seconds: u64,
    },
    LockedDown {
        channel_id: String,
    },
    Unlocked {
        channel_id: String,
    },
}

impl ActionOutcome {
    /// The moderation verb that produced this outcome (legacy `action`).
    #[must_use]
    pub fn action(&self) -> ModerationAction {
        match self {
            Self::Banned { .. } | Self::Unbanned { .. } => ModerationAction::Ban,
            Self::TemporarilyBanned { .. } => ModerationAction::TempBan,
            Self::Kicked { .. } => ModerationAction::Kick,
            Self::TimedOut { .. } => ModerationAction::Timeout,
            Self::Warned { .. } => ModerationAction::Warn,
            Self::Purged { .. } => ModerationAction::Purge,
            Self::SlowmodeUpdated { .. } => ModerationAction::Slowmode,
            Self::LockedDown { .. } => ModerationAction::Lockdown,
            Self::Unlocked { .. } => ModerationAction::Unlock,
        }
    }

    /// Whether this outcome needs no Discord call at all (legacy: warn is
    /// store-only; everything else is a REST verb).
    #[must_use]
    pub fn is_store_only(&self) -> bool {
        matches!(self, Self::Warned { .. })
    }
}

/// One adjudicated moderation execution: the policy-checked request plus the
/// guild/channel context the executor needs (legacy `ModerationExecution` =
/// `ModerationRequest` + `guildId` + `channel` + `requestId` +
/// `idempotencyKey`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationExecution {
    /// Guild the action runs in.
    pub guild_id: String,
    /// Channel for channel verbs and purge/slowmode/lockdown/unlock.
    pub channel_id: Option<String>,
    /// The adjudicated action.
    pub action: ModerationAction,
    /// Member under moderation (member verbs only).
    pub target_user_id: Option<String>,
    /// Validated audit reason (legacy `requireModerationReason`, ≤512 chars).
    pub reason: String,
    /// `tempban`/`timeout` duration in seconds (≥60, legacy sets no max).
    pub duration_seconds: Option<u64>,
    /// `purge` message count (1–100).
    pub count: Option<u64>,
    /// `slowmode` delay in seconds (0–21600).
    pub seconds: Option<u64>,
}

/// Legacy `SEND_MESSAGES` bit (`1 << 11`): the only bit lockdown touches.
/// Stored as decimal strings on the wire (`moderation_lockdowns`
/// `prior_allow`/`prior_deny`), so the helpers below operate on `u64` and the
/// store layer stringifies.
pub const SEND_MESSAGES_BIT: u64 = 2048;

/// Set the send-messages bit (legacy `setBit` in `service.ts`).
#[must_use]
pub fn set_send_bit(mask: u64) -> u64 {
    mask | SEND_MESSAGES_BIT
}

/// Clear the send-messages bit (legacy `clearBit` in `service.ts`).
#[must_use]
pub fn clear_send_bit(mask: u64) -> u64 {
    mask & !SEND_MESSAGES_BIT
}

/// Lockdown overwrite for a channel whose @everyone entry currently reads
/// `(allow, deny)`: deny send, drop any allow (legacy `lockChannel` — every
/// other bit preserved for the eventual unlock row).
#[must_use]
pub fn lockdown_overwrite(allow: u64, deny: u64) -> (u64, u64) {
    (clear_send_bit(allow), set_send_bit(deny))
}

/// Unlock-without-a-record overwrite: clear the send-messages bit on both
/// masks (legacy `unlockChannel` fallback when no lockdown row exists).
#[must_use]
pub fn unlock_overwrite(allow: u64, deny: u64) -> (u64, u64) {
    (clear_send_bit(allow), clear_send_bit(deny))
}

/// How long a 429 parks the executor, in millis (legacy `retryAfterMs` in
/// `kick.ts` and `retry-after * 1000 + 250` in `rest.ts`).
///
/// The body `retry_after` (seconds, float) wins when present and finite;
/// otherwise the `retry-after` header (seconds) stands; garbage or negative
/// values fall back to 1 second. The result adds the legacy 250 ms padding
/// and clamps to [`MAX_RETRY_AFTER_MS`] so a malformed header cannot park a
/// run for a day.
#[must_use]
pub fn retry_after_ms(header_secs: Option<f64>, body_retry_after_secs: Option<f64>) -> u64 {
    let mut seconds = header_secs.unwrap_or(1.0);
    if let Some(body) = body_retry_after_secs {
        if body.is_finite() {
            seconds = body;
        }
    }
    if !seconds.is_finite() || seconds < 0.0 {
        seconds = 1.0;
    }
    ((seconds * 1000.0).ceil() as u64)
        .saturating_add(RETRY_AFTER_PADDING_MS)
        .min(MAX_RETRY_AFTER_MS)
}

/// Legacy 250 ms padding added to every 429 wait (`kick.ts`, `rest.ts`).
pub const RETRY_AFTER_PADDING_MS: u64 = 250;
/// Legacy clamp on one 429 wait (`MAX_RETRY_AFTER_MS` in `kick.ts`).
pub const MAX_RETRY_AFTER_MS: u64 = 60_000;
/// Legacy 5xx / transport backoff base (`500 * 2 ** attempt` in `rest.ts`,
/// `500 * 2 ** (attempts - 1)` in `kick.ts` — the same sequence).
pub const BACKOFF_BASE_MS: u64 = 500;
/// Total HTTP tries per paced call (legacy `kick.ts`: initial attempt + 4
/// retries, `attempts > maxRetries` gives up; `rest.ts` retries 5xx with
/// `attempt >= 4` giving up — the same 5 tries).
pub const MAX_HTTP_TRIES: u32 = 5;

/// 5xx / transport backoff for failure `attempt` (0-based): `500 * 2^attempt`
/// ms — 500, 1000, 2000, 4000, … (legacy `backoff` in both `rest.ts` and
/// `kick.ts`). Saturates instead of overflowing.
#[must_use]
pub fn backoff_ms(attempt: u32) -> u64 {
    BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(20))
}

/// How long to wait before the next paced request: the gap between the
/// minimum interval after the last request and now, or zero when the floor
/// already passed (legacy `pace()` in `rest.ts` / `kick.ts`, made pure).
#[must_use]
pub fn pace_wait_ms(last_at_ms: u64, min_interval_ms: u64, now_ms: u64) -> u64 {
    last_at_ms
        .saturating_add(min_interval_ms)
        .saturating_sub(now_ms)
}

/// Terminal kick status classes (legacy `DiscordKicker::kick` branches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickStatus {
    /// 200/204 — the member is gone.
    Removed,
    /// 404 — not a member; terminal, not a failure.
    AlreadyGone,
    /// 403 — missing permission or hierarchy; retrying will not help.
    Forbidden,
    /// 401 — the token was rejected.
    Unauthorized,
    /// 429 — needs a `retry-after` wait; terminal only past the budget.
    RateLimited,
    /// 5xx — retryable; terminal only past the budget.
    ServerError,
    /// Anything else — never retried.
    Other,
}

/// Classify one kick HTTP status (legacy branch order in `kick.ts`: success,
/// 404, 403, 401, 429, 5xx, else).
#[must_use]
pub fn classify_kick_status(status: u16) -> KickStatus {
    match status {
        200 | 204 => KickStatus::Removed,
        404 => KickStatus::AlreadyGone,
        403 => KickStatus::Forbidden,
        401 => KickStatus::Unauthorized,
        429 => KickStatus::RateLimited,
        500..=599 => KickStatus::ServerError,
        _ => KickStatus::Other,
    }
}

/// Kick outcome (legacy `KickOutcome` in `kick.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickOutcome {
    Kicked,
    AlreadyGone,
    Forbidden,
    RateLimited,
    Failed,
}

/// One removal result (legacy `KickResult` in `kick.ts`): every ending is a
/// value — the executor never throws — because the caller owes this member
/// an audit line either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickResult {
    pub outcome: KickOutcome,
    /// HTTP status, or `None` when the request never produced a response.
    pub status: Option<u16>,
    /// Short, non-secret reason for the audit line.
    pub detail: String,
    /// HTTP attempts spent, including retries.
    pub attempts: u32,
}

/// Parse a `retry-after`-style seconds value (header or JSON body field).
/// Returns `None` for missing, empty, or non-numeric input — the caller falls
/// back to the legacy 1-second default via [`retry_after_ms`].
#[must_use]
pub fn parse_retry_after_secs(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|s| {
        let v: f64 = s.trim().parse().ok()?;
        Some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_action_round_trips() {
        let cases = [
            (
                ActionOutcome::Banned {
                    user_id: "1".to_owned(),
                },
                ModerationAction::Ban,
                false,
            ),
            (
                ActionOutcome::TemporarilyBanned {
                    user_id: "1".to_owned(),
                    duration_seconds: 3600,
                },
                ModerationAction::TempBan,
                false,
            ),
            (
                ActionOutcome::Kicked {
                    user_id: "1".to_owned(),
                },
                ModerationAction::Kick,
                false,
            ),
            (
                ActionOutcome::TimedOut {
                    user_id: "1".to_owned(),
                    duration_seconds: 600,
                },
                ModerationAction::Timeout,
                false,
            ),
            (
                ActionOutcome::Warned {
                    user_id: "1".to_owned(),
                },
                ModerationAction::Warn,
                true,
            ),
            (
                ActionOutcome::Unbanned {
                    user_id: "1".to_owned(),
                },
                ModerationAction::Ban,
                false,
            ),
            (
                ActionOutcome::Purged {
                    channel_id: "9".to_owned(),
                    affected: 7,
                },
                ModerationAction::Purge,
                false,
            ),
            (
                ActionOutcome::SlowmodeUpdated {
                    channel_id: "9".to_owned(),
                    seconds: 30,
                },
                ModerationAction::Slowmode,
                false,
            ),
            (
                ActionOutcome::LockedDown {
                    channel_id: "9".to_owned(),
                },
                ModerationAction::Lockdown,
                false,
            ),
            (
                ActionOutcome::Unlocked {
                    channel_id: "9".to_owned(),
                },
                ModerationAction::Unlock,
                false,
            ),
        ];
        for (outcome, action, store_only) in cases {
            assert_eq!(outcome.action(), action);
            assert_eq!(outcome.is_store_only(), store_only);
        }
    }

    #[test]
    fn send_bit_helpers_match_legacy() {
        assert_eq!(SEND_MESSAGES_BIT, 2048);
        // Lock: deny gains the bit, allow loses it, others preserved.
        assert_eq!(lockdown_overwrite(0, 0), (0, 2048));
        assert_eq!(lockdown_overwrite(2048 | 1024, 64), (1024, 64 | 2048));
        // Unlock fallback: bit cleared on both masks.
        assert_eq!(unlock_overwrite(2048 | 1024, 2048 | 64), (1024, 64));
        assert_eq!(set_send_bit(0), 2048);
        assert_eq!(clear_send_bit(2048 | 8), 8);
    }

    #[test]
    fn retry_after_matches_legacy_numbers() {
        // Header-only: seconds * 1000 + 250 (rest.ts).
        assert_eq!(retry_after_ms(Some(1.0), None), 1250);
        assert_eq!(retry_after_ms(Some(2.0), None), 2250);
        // Body wins when present (kick.ts); fractional seconds ceil.
        assert_eq!(retry_after_ms(Some(5.0), Some(1.5)), 1750);
        assert_eq!(retry_after_ms(Some(1.0), Some(6.457)), 6707);
        // Missing header defaults to 1s + 250.
        assert_eq!(retry_after_ms(None, None), 1250);
        // Garbage / negative falls back to 1s + 250.
        assert_eq!(retry_after_ms(Some(f64::NAN), None), 1250);
        assert_eq!(retry_after_ms(Some(-3.0), None), 1250);
        // Non-finite body keeps the header.
        assert_eq!(retry_after_ms(Some(2.0), Some(f64::INFINITY)), 2250);
        // Clamp: a malformed retry-after of 86400 must not park the run.
        assert_eq!(retry_after_ms(Some(86_400.0), None), MAX_RETRY_AFTER_MS);
        assert_eq!(retry_after_ms(Some(120.0), None), MAX_RETRY_AFTER_MS);
    }

    #[test]
    fn backoff_matches_legacy_sequence() {
        // 500 * 2^attempt: rest.ts `500 * 2 ** attempt`, kick.ts identical.
        assert_eq!(
            (0..5).map(backoff_ms).collect::<Vec<_>>(),
            [500, 1000, 2000, 4000, 8000]
        );
        assert_eq!(MAX_HTTP_TRIES, 5);
    }

    #[test]
    fn pace_floor_matches_legacy() {
        // last + interval - now, floored at zero.
        assert_eq!(pace_wait_ms(1000, 110, 1050), 60);
        assert_eq!(pace_wait_ms(1000, 110, 1200), 0);
        assert_eq!(pace_wait_ms(1000, 350, 1100), 250);
    }

    #[test]
    fn kick_status_branches_match_legacy() {
        assert_eq!(classify_kick_status(200), KickStatus::Removed);
        assert_eq!(classify_kick_status(204), KickStatus::Removed);
        assert_eq!(classify_kick_status(404), KickStatus::AlreadyGone);
        assert_eq!(classify_kick_status(403), KickStatus::Forbidden);
        assert_eq!(classify_kick_status(401), KickStatus::Unauthorized);
        assert_eq!(classify_kick_status(429), KickStatus::RateLimited);
        assert_eq!(classify_kick_status(500), KickStatus::ServerError);
        assert_eq!(classify_kick_status(503), KickStatus::ServerError);
        assert_eq!(classify_kick_status(400), KickStatus::Other);
        assert_eq!(classify_kick_status(418), KickStatus::Other);
    }

    #[test]
    fn parse_retry_after_secs_rejects_garbage() {
        assert_eq!(parse_retry_after_secs(Some("1")), Some(1.0));
        assert_eq!(parse_retry_after_secs(Some(" 6.457 ")), Some(6.457));
        assert_eq!(parse_retry_after_secs(None), None);
        assert_eq!(parse_retry_after_secs(Some("")), None);
        assert_eq!(parse_retry_after_secs(Some("soon")), None);
    }
}
