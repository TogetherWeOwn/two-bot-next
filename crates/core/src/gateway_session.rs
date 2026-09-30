//! Framework-free gateway checkpoint policy. Sequence is the last **committed**
//! dispatch, not merely the last packet received by the websocket library.

/// A deliberately conservative local freshness budget, not a Discord promise
/// about how long a gateway session remains resumable.
pub const SESSION_MAX_AGE_MS: i64 = 15 * 60 * 1000;

#[derive(Clone, PartialEq, Eq)]
pub struct GatewaySession {
    pub session_id: String,
    pub sequence: u64,
    pub resume_url: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootAction {
    Identify,
    Resume,
    DiscardAndIdentify,
}

pub fn boot_action(saved: Option<&GatewaySession>, now_ms: i64) -> BootAction {
    let Some(saved) = saved else {
        return BootAction::Identify;
    };
    let age = now_ms.checked_sub(saved.updated_at_ms);
    if saved.session_id.is_empty()
        || saved.resume_url.is_empty()
        || !matches!(age, Some(0..=SESSION_MAX_AGE_MS))
    {
        BootAction::DiscardAndIdentify
    } else {
        BootAction::Resume
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchAction {
    Apply,
    Duplicate,
}

pub fn dispatch_action(
    saved: Option<&GatewaySession>,
    session_id: &str,
    sequence: u64,
) -> DispatchAction {
    if saved.is_some_and(|s| s.session_id == session_id && sequence <= s.sequence) {
        DispatchAction::Duplicate
    } else {
        DispatchAction::Apply
    }
}

/// Opcode 9 only invalidates durable state when Discord says it cannot resume.
pub const fn invalidates_session(resumable: bool) -> bool {
    !resumable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved() -> GatewaySession {
        GatewaySession {
            session_id: "mock-session".into(),
            sequence: 42,
            resume_url: "ws://mock-gateway".into(),
            updated_at_ms: 1000,
        }
    }

    #[test]
    fn restart_uses_committed_sequence() {
        let session = saved();
        assert_eq!(boot_action(Some(&session), 2000), BootAction::Resume);
        assert_eq!(session.sequence, 42);
        assert_eq!(boot_action(None, 2000), BootAction::Identify);
    }

    #[test]
    fn expired_and_future_checkpoints_are_discarded() {
        let session = saved();
        assert_eq!(
            boot_action(Some(&session), 1000 + SESSION_MAX_AGE_MS),
            BootAction::Resume
        );
        assert_eq!(
            boot_action(Some(&session), 1001 + SESSION_MAX_AGE_MS),
            BootAction::DiscardAndIdentify
        );
        assert_eq!(
            boot_action(Some(&session), 999),
            BootAction::DiscardAndIdentify
        );
        assert_eq!(
            boot_action(Some(&session), i64::MAX),
            BootAction::DiscardAndIdentify
        );
    }

    #[test]
    fn replay_is_skipped_but_new_session_can_reset_sequence() {
        let session = saved();
        assert_eq!(
            dispatch_action(Some(&session), "mock-session", 42),
            DispatchAction::Duplicate
        );
        assert_eq!(
            dispatch_action(Some(&session), "mock-session", 41),
            DispatchAction::Duplicate
        );
        assert_eq!(
            dispatch_action(Some(&session), "mock-session", 43),
            DispatchAction::Apply
        );
        assert_eq!(
            dispatch_action(Some(&session), "new-session", 1),
            DispatchAction::Apply
        );
    }

    #[test]
    fn invalid_session_only_falls_back_when_not_resumable() {
        assert!(invalidates_session(false));
        assert!(!invalidates_session(true));
    }
}
