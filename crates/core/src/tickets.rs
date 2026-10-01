//! Framework-free tickets contract, ported from legacy `src/discord/tickets.ts`.
//! The interaction router owns acknowledgement; the shared REST executor owns
//! channel/permission/message calls. A transcript must be durable before delete.

use crate::commands::PERM_MANAGE_CHANNELS;
use crate::funnel::format_iso_millis;

pub const TICKET_OPEN_ID: &str = "two:tickets:open";
pub const TICKET_CLAIM_ID: &str = "two:tickets:claim";
pub const TICKET_CLOSE_ID: &str = "two:tickets:close";
pub const COOLDOWN_SECONDS: u64 = 300;
pub const RECOVERY_INTERVAL_SECONDS: u64 = 300;
pub const PURGE_INTERVAL_SECONDS: u64 = 3600;
pub const INTERRUPTED_AFTER_MS: i64 = 15 * 60 * 1000;
pub const TRANSCRIPT_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
pub const MAX_TRANSCRIPT_UTF16_UNITS: usize = 200_000;
pub const PANEL_TEXT: &str =
    "**Support tickets**\nOpen a private ticket and a staff member will help you.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketAction {
    Open,
    Claim,
    Close,
}

impl TicketAction {
    #[must_use]
    pub fn from_custom_id(id: &str) -> Option<Self> {
        match id {
            TICKET_OPEN_ID => Some(Self::Open),
            TICKET_CLAIM_ID => Some(Self::Claim),
            TICKET_CLOSE_ID => Some(Self::Close),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketStatus {
    Creating,
    Open,
    Closing,
    CleanupPending,
    Closed,
}

impl TicketStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Open => "open",
            Self::Closing => "closing",
            Self::CleanupPending => "cleanup_pending",
            Self::Closed => "closed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "creating" => Some(Self::Creating),
            "open" => Some(Self::Open),
            "closing" => Some(Self::Closing),
            "cleanup_pending" => Some(Self::CleanupPending),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub guild_id: String,
    pub channel_id: Option<String>,
    pub opener_id: String,
    pub claimed_by: Option<String>,
    pub status: TicketStatus,
    pub created_at: i64,
    pub closing_started_at: Option<i64>,
    pub closed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    #[error("Tickets are only available in the configured guild")]
    WrongGuild,
    #[error("Only staff can claim or close tickets")]
    StaffOnly,
    #[error("This ticket is already claimed or not open")]
    AlreadyClaimed,
    #[error("Ticket state changed; reload it before continuing")]
    InvalidTransition,
    #[error("The ticket channel is unavailable")]
    MissingChannel,
    #[error("This ticket close was already recovered or completed")]
    StaleClose,
}

/// Roles and permissions must come from the invoking guild member, never options.
/// Discord Administrator implies ManageChannels even when its bit is not present.
pub fn authorize(
    guild_id: Option<&str>,
    configured_guild: &str,
    action: TicketAction,
    role_ids: &[String],
    permissions: u64,
    staff_role: &str,
) -> Result<(), TicketError> {
    if configured_guild.is_empty() || guild_id != Some(configured_guild) {
        return Err(TicketError::WrongGuild);
    }
    if action != TicketAction::Open
        && permissions & (PERM_MANAGE_CHANNELS | (1 << 3)) == 0
        && !role_ids.iter().any(|id| id == staff_role)
    {
        return Err(TicketError::StaffOnly);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDecision {
    Existing { channel_id: Option<String> },
    Cooldown,
    Reserve,
}

/// The store calls this inside the per-(guild, opener) reservation transaction.
#[must_use]
pub fn decide_open(
    active: Option<&Ticket>,
    last_created_at: Option<i64>,
    now: i64,
    cooldown_seconds: u64,
) -> OpenDecision {
    if let Some(ticket) = active {
        return OpenDecision::Existing {
            channel_id: ticket.channel_id.clone(),
        };
    }
    let cooldown_ms = i64::try_from(cooldown_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
    if last_created_at.is_some_and(|last| now.saturating_sub(last) < cooldown_ms) {
        OpenDecision::Cooldown
    } else {
        OpenDecision::Reserve
    }
}

impl Ticket {
    pub fn record_channel(&mut self, channel_id: &str) -> Result<(), TicketError> {
        if self.status != TicketStatus::Creating
            || self
                .channel_id
                .as_deref()
                .is_some_and(|id| id != channel_id)
            || channel_id.is_empty()
        {
            return Err(TicketError::InvalidTransition);
        }
        self.channel_id = Some(channel_id.to_owned());
        Ok(())
    }

    pub fn activate(&mut self, channel_id: &str) -> Result<(), TicketError> {
        self.record_channel(channel_id)?;
        self.status = TicketStatus::Open;
        Ok(())
    }

    pub fn claim(&mut self, staff_id: &str) -> Result<(), TicketError> {
        if self.status != TicketStatus::Open || self.claimed_by.is_some() || staff_id.is_empty() {
            return Err(TicketError::AlreadyClaimed);
        }
        self.claimed_by = Some(staff_id.to_owned());
        Ok(())
    }

    pub fn begin_close(&mut self, now: i64) -> Result<(), TicketError> {
        if self.status != TicketStatus::Open {
            return Err(TicketError::InvalidTransition);
        }
        if self.channel_id.is_none() {
            return Err(TicketError::MissingChannel);
        }
        self.status = TicketStatus::Closing;
        self.closing_started_at = Some(now);
        Ok(())
    }

    fn check_close(&self, expected_started_at: i64) -> Result<(), TicketError> {
        if self.status != TicketStatus::Closing
            || self.closing_started_at != Some(expected_started_at)
        {
            return Err(TicketError::StaleClose);
        }
        Ok(())
    }

    /// Called only after the REST executor freezes opener writes and completes
    /// pagination. The store persists this snapshot and state change atomically.
    pub fn capture_close(
        &mut self,
        expected_started_at: i64,
        captured_at: i64,
        snapshot: TranscriptSnapshot,
    ) -> Result<Transcript, TicketError> {
        self.check_close(expected_started_at)?;
        let channel_id = self.channel_id.clone().ok_or(TicketError::MissingChannel)?;
        let transcript = Transcript {
            ticket_id: self.id.clone(),
            guild_id: self.guild_id.clone(),
            channel_id,
            opener_id: self.opener_id.clone(),
            claimed_by: self.claimed_by.clone(),
            content: snapshot.content,
            message_count: snapshot.message_count,
            created_at: captured_at,
            purge_after: captured_at.saturating_add(TRANSCRIPT_RETENTION_MS),
        };
        self.status = TicketStatus::CleanupPending;
        self.closed_at = Some(captured_at);
        Ok(transcript)
    }

    /// Recovery must restore opener permissions BEFORE calling this; an old
    /// closer is fenced by closing_started_at and cannot save after recovery.
    pub fn reopen_interrupted(
        &mut self,
        expected_started_at: i64,
        transcript_exists: bool,
    ) -> Result<(), TicketError> {
        self.check_close(expected_started_at)?;
        if transcript_exists {
            return Err(TicketError::InvalidTransition);
        }
        self.status = TicketStatus::Open;
        self.closing_started_at = None;
        Ok(())
    }

    /// Handles legacy crashes between transcript INSERT and state UPDATE.
    pub fn recover_saved_close(
        &mut self,
        expected_started_at: i64,
        captured_at: i64,
    ) -> Result<(), TicketError> {
        self.check_close(expected_started_at)?;
        self.status = TicketStatus::CleanupPending;
        self.closed_at = Some(captured_at);
        Ok(())
    }

    /// Confirms a capture whose transcript body was retention-purged after
    /// the store reconciled the row to cleanup. Idempotent: succeeds only
    /// when the row already carries the reconciled state for this close
    /// token, so a purged body can never reopen a captured close.
    pub fn confirm_reconciled_close(
        &mut self,
        expected_started_at: i64,
    ) -> Result<(), TicketError> {
        if self.status != TicketStatus::CleanupPending
            || self.closing_started_at != Some(expected_started_at)
            || self.closed_at.is_none()
        {
            return Err(TicketError::StaleClose);
        }
        Ok(())
    }

    /// Fenced completion for an unsaved close whose channel is confirmed
    /// absent. Call only after the REST executor proves the channel is gone
    /// (successful DELETE or Discord code 10003) and no transcript row was
    /// captured. The expected close token fences a racing `capture_close`,
    /// mirroring `reopen_interrupted`/`recover_saved_close`; legacy
    /// `recoverClosing` completed this 10003 path without a transcript.
    pub fn abandon_unsaved_close(
        &mut self,
        expected_started_at: i64,
        now: i64,
    ) -> Result<(), TicketError> {
        self.check_close(expected_started_at)?;
        self.status = TicketStatus::Closed;
        self.closed_at = Some(now);
        Ok(())
    }

    /// Failed creation/control posting must remain recoverable until deletion
    /// succeeds. Unlike a normal close, an open rollback has no transcript.
    pub fn queue_open_rollback(&mut self, now: i64) -> Result<(), TicketError> {
        if !matches!(self.status, TicketStatus::Creating | TicketStatus::Open) {
            return Err(TicketError::InvalidTransition);
        }
        if self.channel_id.is_none() {
            return Err(TicketError::MissingChannel);
        }
        self.status = TicketStatus::CleanupPending;
        self.closed_at = Some(now);
        Ok(())
    }

    /// Only a successful DELETE or Discord code 10003 may call this.
    pub fn finish_cleanup(&mut self, now: i64) -> Result<(), TicketError> {
        if self.status != TicketStatus::CleanupPending {
            return Err(TicketError::InvalidTransition);
        }
        self.status = TicketStatus::Closed;
        self.closed_at = Some(self.closed_at.unwrap_or(now));
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptMessage {
    pub created_at: i64,
    pub author_tag: String,
    pub content: String,
    pub attachment_urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSnapshot {
    pub content: String,
    pub message_count: i32,
}

/// Legacy chronological format; UTF-16 limit matches JavaScript string.length.
/// Input is the complete paginated result, including attachment URLs.
#[must_use]
pub fn format_transcript(mut messages: Vec<TranscriptMessage>) -> TranscriptSnapshot {
    messages.sort_by_key(|message| message.created_at);
    let message_count = i32::try_from(messages.len()).unwrap_or(i32::MAX);
    let lines: Vec<_> = messages
        .into_iter()
        .map(|message| {
            let attachments = if message.attachment_urls.is_empty() {
                String::new()
            } else {
                format!(" {}", message.attachment_urls.join(" "))
            };
            format!(
                "[{}] {}: {}{}",
                format_iso_millis(message.created_at),
                message.author_tag,
                message.content,
                attachments
            )
            .trim()
            .to_owned()
        })
        .collect();
    let mut content = lines.join("\n");
    let mut units = 0;
    let mut truncate_at = None;
    for (index, ch) in content.char_indices() {
        units += ch.len_utf16();
        if units > MAX_TRANSCRIPT_UTF16_UNITS {
            truncate_at = Some(index);
            break;
        }
    }
    if let Some(index) = truncate_at {
        content.truncate(index);
        content.push_str("\n[transcript truncated]");
    }
    TranscriptSnapshot {
        content,
        message_count,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub ticket_id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub opener_id: String,
    pub claimed_by: Option<String>,
    pub content: String,
    pub message_count: i32,
    pub created_at: i64,
    pub purge_after: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// On Ready, ensure controls exist, without reposting ones already present.
    ReattachOpenControls {
        channel_id: String,
    },
    FindCreatingChannel {
        topic: String,
    },
    DeleteInterruptedCreate {
        channel_id: String,
    },
    RestoreOpenerThenReopen {
        channel_id: String,
        started_at: i64,
    },
    RecoverSavedClose {
        started_at: i64,
    },
    RetryCleanup {
        channel_id: String,
    },
    None,
}

/// No DB or Discord lookups here. Missing/forbidden channel queries are NOT
/// absence evidence; the adapter must leave state intact and retry later.
#[must_use]
pub fn recovery_action(ticket: &Ticket, now: i64, transcript_exists: bool) -> RecoveryAction {
    match ticket.status {
        TicketStatus::Creating if now.saturating_sub(ticket.created_at) >= INTERRUPTED_AFTER_MS => {
            match &ticket.channel_id {
                Some(id) => RecoveryAction::DeleteInterruptedCreate {
                    channel_id: id.clone(),
                },
                None => RecoveryAction::FindCreatingChannel {
                    topic: format!("two-ticket:{}", ticket.id),
                },
            }
        }
        TicketStatus::Open => ticket
            .channel_id
            .as_ref()
            .map_or(RecoveryAction::None, |id| {
                RecoveryAction::ReattachOpenControls {
                    channel_id: id.clone(),
                }
            }),
        TicketStatus::Closing => {
            let Some(started_at) = ticket.closing_started_at else {
                return RecoveryAction::None;
            };
            if now.saturating_sub(started_at) < INTERRUPTED_AFTER_MS {
                return RecoveryAction::None;
            }
            if transcript_exists {
                RecoveryAction::RecoverSavedClose { started_at }
            } else {
                ticket
                    .channel_id
                    .as_ref()
                    .map_or(RecoveryAction::None, |id| {
                        RecoveryAction::RestoreOpenerThenReopen {
                            channel_id: id.clone(),
                            started_at,
                        }
                    })
            }
        }
        TicketStatus::CleanupPending => {
            ticket
                .channel_id
                .as_ref()
                .map_or(RecoveryAction::None, |id| RecoveryAction::RetryCleanup {
                    channel_id: id.clone(),
                })
        }
        _ => RecoveryAction::None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelMessage {
    pub author_id: String,
    pub custom_ids: Vec<String>,
}

/// Call with the panel channel's most recent 50 messages, as in legacy ensurePanel.
#[must_use]
pub fn panel_needed(bot_id: &str, messages: &[PanelMessage]) -> bool {
    !messages.iter().any(|message| {
        message.author_id == bot_id && message.custom_ids.iter().any(|id| id == TICKET_OPEN_ID)
    })
}

#[must_use]
pub fn ticket_channel_name(username: &str) -> String {
    let lower = username.to_lowercase();
    let mut safe = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
            safe.push(ch);
        } else if !safe.ends_with('-') {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches('-');
    let safe = &safe[..safe.len().min(24)];
    format!("ticket-{}", if safe.is_empty() { "member" } else { safe })
}

#[must_use]
pub fn is_unknown_channel(discord_code: Option<u64>) -> bool {
    discord_code == Some(10003)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        Ticket {
            id: "reservation".into(),
            guild_id: "guild".into(),
            channel_id: None,
            opener_id: "member".into(),
            claimed_by: None,
            status: TicketStatus::Creating,
            created_at: 0,
            closing_started_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn lifecycle_captures_before_delete_and_never_overwrites_claim() {
        let mut t = ticket();
        assert_eq!(t.begin_close(10), Err(TicketError::InvalidTransition));
        t.record_channel("channel").unwrap();
        assert_eq!(t.activate("different"), Err(TicketError::InvalidTransition));
        t.activate("channel").unwrap();
        t.claim("staff").unwrap();
        assert_eq!(t.claim("other"), Err(TicketError::AlreadyClaimed));
        t.begin_close(10).unwrap();
        assert_eq!(t.finish_cleanup(20), Err(TicketError::InvalidTransition));
        let transcript = t.capture_close(10, 20, format_transcript(vec![])).unwrap();
        assert_eq!(transcript.claimed_by.as_deref(), Some("staff"));
        assert_eq!(transcript.purge_after, 20 + TRANSCRIPT_RETENTION_MS);
        assert_eq!(t.status, TicketStatus::CleanupPending);
        t.finish_cleanup(30).unwrap();
        assert_eq!(t.closed_at, Some(20));
        assert_eq!(t.begin_close(40), Err(TicketError::InvalidTransition));
    }

    #[test]
    fn guild_and_staff_gates_apply_to_buttons() {
        for action in [TicketAction::Open, TicketAction::Claim, TicketAction::Close] {
            assert_eq!(
                authorize(None, "guild", action, &[], u64::MAX, "staff"),
                Err(TicketError::WrongGuild)
            );
            assert_eq!(
                authorize(
                    Some("other"),
                    "guild",
                    action,
                    &["staff".into()],
                    0,
                    "staff"
                ),
                Err(TicketError::WrongGuild)
            );
        }
        assert!(authorize(Some("guild"), "guild", TicketAction::Open, &[], 0, "staff").is_ok());
        for action in [TicketAction::Claim, TicketAction::Close] {
            assert_eq!(
                authorize(Some("guild"), "guild", action, &[], 0, "staff"),
                Err(TicketError::StaffOnly)
            );
            assert!(authorize(
                Some("guild"),
                "guild",
                action,
                &["staff".into()],
                0,
                "staff"
            )
            .is_ok());
            assert!(authorize(
                Some("guild"),
                "guild",
                action,
                &[],
                PERM_MANAGE_CHANNELS,
                "staff"
            )
            .is_ok());
            assert!(authorize(Some("guild"), "guild", action, &[], 1 << 3, "staff").is_ok());
        }
        assert_eq!(
            TicketAction::from_custom_id(TICKET_OPEN_ID),
            Some(TicketAction::Open)
        );
        assert_eq!(
            TicketAction::from_custom_id(TICKET_CLAIM_ID),
            Some(TicketAction::Claim)
        );
        assert_eq!(
            TicketAction::from_custom_id(TICKET_CLOSE_ID),
            Some(TicketAction::Close)
        );
        assert_eq!(TicketAction::from_custom_id("two:tickets:bogus"), None);
    }

    #[test]
    fn cooldown_is_300_seconds_with_exact_boundary_and_active_precedence() {
        assert_eq!(
            decide_open(None, None, 0, COOLDOWN_SECONDS),
            OpenDecision::Reserve
        );
        assert_eq!(
            decide_open(None, Some(0), 299_999, COOLDOWN_SECONDS),
            OpenDecision::Cooldown
        );
        assert_eq!(
            decide_open(None, Some(0), 300_000, COOLDOWN_SECONDS),
            OpenDecision::Reserve
        );
        assert_eq!(
            decide_open(None, Some(1), 0, COOLDOWN_SECONDS),
            OpenDecision::Cooldown
        );
        assert_eq!(
            decide_open(Some(&ticket()), Some(0), 1, COOLDOWN_SECONDS),
            OpenDecision::Existing { channel_id: None }
        );
    }

    #[test]
    fn restart_reattaches_open_and_recovers_only_stale_work() {
        let mut t = ticket();
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS - 1, false),
            RecoveryAction::None
        );
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS, false),
            RecoveryAction::FindCreatingChannel {
                topic: "two-ticket:reservation".into()
            }
        );
        t.record_channel("channel").unwrap();
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS, false),
            RecoveryAction::DeleteInterruptedCreate {
                channel_id: "channel".into()
            }
        );
        t.activate("channel").unwrap();
        assert_eq!(
            recovery_action(&t, 0, false),
            RecoveryAction::ReattachOpenControls {
                channel_id: "channel".into()
            }
        );
        t.begin_close(10).unwrap();
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS + 9, false),
            RecoveryAction::None
        );
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS + 10, false),
            RecoveryAction::RestoreOpenerThenReopen {
                channel_id: "channel".into(),
                started_at: 10
            }
        );
        assert_eq!(
            recovery_action(&t, INTERRUPTED_AFTER_MS + 10, true),
            RecoveryAction::RecoverSavedClose { started_at: 10 }
        );
        assert_eq!(
            (RECOVERY_INTERVAL_SECONDS, PURGE_INTERVAL_SECONDS),
            (300, 3600)
        );
    }

    #[test]
    fn recovery_fences_late_transcript_and_refuses_reopening_saved_close() {
        let mut t = ticket();
        t.activate("channel").unwrap();
        t.begin_close(10).unwrap();
        assert_eq!(
            t.reopen_interrupted(11, false),
            Err(TicketError::StaleClose)
        );
        assert_eq!(
            t.reopen_interrupted(10, true),
            Err(TicketError::InvalidTransition)
        );
        t.reopen_interrupted(10, false).unwrap();
        t.begin_close(20).unwrap();
        assert_eq!(
            t.capture_close(10, 30, format_transcript(vec![])),
            Err(TicketError::StaleClose)
        );
        t.recover_saved_close(20, 30).unwrap();
        assert_eq!(
            recovery_action(&t, 40, true),
            RecoveryAction::RetryCleanup {
                channel_id: "channel".into()
            }
        );
        t.finish_cleanup(40).unwrap();
        assert_eq!(recovery_action(&t, 50, true), RecoveryAction::None);
    }

    #[test]
    fn unsaved_close_with_confirmed_absent_channel_completes_without_transcript() {
        let mut t = ticket();
        t.activate("channel").unwrap();
        t.begin_close(10).unwrap();
        // Wrong close token stays fenced, mirroring reopen/recover fencing.
        assert_eq!(
            t.abandon_unsaved_close(11, 20),
            Err(TicketError::StaleClose)
        );
        t.abandon_unsaved_close(10, 20).unwrap();
        assert_eq!(t.status, TicketStatus::Closed);
        assert_eq!(t.closed_at, Some(20));
        // Terminal: the opener may open a fresh ticket afterwards.
        assert_eq!(
            decide_open(None, Some(0), 300_000, COOLDOWN_SECONDS),
            OpenDecision::Reserve
        );
        // A captured close can no longer be abandoned.
        let mut saved = ticket();
        saved.activate("channel").unwrap();
        saved.begin_close(10).unwrap();
        saved
            .capture_close(10, 30, format_transcript(vec![]))
            .unwrap();
        assert_eq!(
            saved.abandon_unsaved_close(10, 40),
            Err(TicketError::StaleClose)
        );
    }

    #[test]
    fn reconciled_close_confirms_without_body_and_never_reopens() {
        // `purge_expired` reconciles a legacy crash row to cleanup in the
        // same commit that deletes the body; the row is the capture marker.
        let mut t = ticket();
        t.activate("channel").unwrap();
        t.begin_close(10).unwrap();
        t.recover_saved_close(10, 20).unwrap();
        t.confirm_reconciled_close(10).unwrap();
        // The row is CleanupPending, not Closing: check_close fences the
        // reopen as a stale close, so a purged body can never reopen it.
        assert_eq!(
            t.reopen_interrupted(10, false),
            Err(TicketError::StaleClose)
        );
        // Anything else is still a stale close, not a recovered one.
        let mut other = ticket();
        other.activate("channel").unwrap();
        other.begin_close(10).unwrap();
        assert_eq!(
            other.confirm_reconciled_close(11),
            Err(TicketError::StaleClose)
        );
        assert_eq!(
            other.confirm_reconciled_close(10),
            Err(TicketError::StaleClose)
        );
    }

    #[test]
    fn transcript_orders_messages_and_preserves_attachments() {
        let snapshot = format_transcript(vec![
            TranscriptMessage {
                created_at: 1000,
                author_tag: "later".into(),
                content: "world".into(),
                attachment_urls: vec!["https://example.test/file".into()],
            },
            TranscriptMessage {
                created_at: 0,
                author_tag: "first".into(),
                content: "hello".into(),
                attachment_urls: vec![],
            },
        ]);
        assert_eq!(snapshot.message_count, 2);
        assert_eq!(snapshot.content, "[1970-01-01T00:00:00.000Z] first: hello\n[1970-01-01T00:00:01.000Z] later: world https://example.test/file");
        assert_eq!(format_transcript(vec![]).message_count, 0);
    }

    #[test]
    fn transcript_truncation_is_unicode_safe_and_keeps_total_count() {
        let snapshot = format_transcript(vec![TranscriptMessage {
            created_at: 0,
            author_tag: "a".into(),
            content: "😀".repeat(110_000),
            attachment_urls: vec![],
        }]);
        assert_eq!(snapshot.message_count, 1);
        assert!(snapshot.content.ends_with("\n[transcript truncated]"));
        let retained = snapshot
            .content
            .strip_suffix("\n[transcript truncated]")
            .unwrap();
        assert!(retained.encode_utf16().count() <= MAX_TRANSCRIPT_UTF16_UNITS);
    }

    #[test]
    fn panel_ensure_ignores_other_authors_and_channel_names_match_legacy() {
        let message = PanelMessage {
            author_id: "other".into(),
            custom_ids: vec![TICKET_OPEN_ID.into()],
        };
        assert!(panel_needed("bot", &[message]));
        let message = PanelMessage {
            author_id: "bot".into(),
            custom_ids: vec![TICKET_OPEN_ID.into()],
        };
        assert!(!panel_needed("bot", &[message]));
        assert_eq!(
            ticket_channel_name("A very unsafe Username!!!"),
            "ticket-a-very-unsafe-username"
        );
        assert_eq!(ticket_channel_name("!!!"), "ticket-member");
        assert_eq!(ticket_channel_name(&"x".repeat(40)).len(), 31);
        assert!(is_unknown_channel(Some(10003)));
        assert!(!is_unknown_channel(Some(50013)));
        assert!(!is_unknown_channel(None));
    }
}
