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

/// One-shot operator input read with the checkpoint at boot. `None` keeps the
/// age policy above; an armed `ForceIdentify` never offers RESUME.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BootDirective {
    #[default]
    None,
    ForceIdentify,
}

pub fn boot_action_with(
    saved: Option<&GatewaySession>,
    directive: BootDirective,
    now_ms: i64,
) -> BootAction {
    match directive {
        BootDirective::ForceIdentify => BootAction::DiscardAndIdentify,
        BootDirective::None => boot_action(saved, now_ms),
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

/// Dispatches Discord assigned but this process never received in one session:
/// the gap between the last received checkpoint and a new sequence. Zero for
/// a new session, for duplicates, and for the next expected sequence. A
/// nonzero gap means transport loss inside a resumable session; the watch
/// counts it toward the zero-missed-events acceptance.
pub fn missed_gap(prev: Option<&GatewaySession>, session_id: &str, sequence: u64) -> u64 {
    match prev {
        Some(saved) if saved.session_id == session_id => {
            sequence.saturating_sub(saved.sequence.saturating_add(1))
        }
        _ => 0,
    }
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
    fn armed_directive_always_identifies_and_unarmed_keeps_age_policy() {
        use BootAction::{DiscardAndIdentify, Identify, Resume};
        let at = |updated_at_ms| GatewaySession {
            updated_at_ms,
            ..saved()
        };
        let empty = GatewaySession {
            session_id: String::new(),
            ..saved()
        };
        let now = 2000;
        let cases = [
            ("no checkpoint", None, Identify),
            ("fresh", Some(at(now)), Resume),
            ("oldest fresh", Some(at(now - SESSION_MAX_AGE_MS)), Resume),
            (
                "stale",
                Some(at(now - SESSION_MAX_AGE_MS - 1)),
                DiscardAndIdentify,
            ),
            ("future-dated", Some(at(now + 1)), DiscardAndIdentify),
            ("empty session_id", Some(empty), DiscardAndIdentify),
        ];
        for (state, checkpoint, unarmed) in cases {
            let checkpoint = checkpoint.as_ref();
            assert_eq!(
                boot_action_with(checkpoint, BootDirective::None, now),
                unarmed,
                "unarmed {state}"
            );
            assert_eq!(
                boot_action_with(checkpoint, BootDirective::None, now),
                boot_action(checkpoint, now),
                "unarmed {state} matches the age policy"
            );
            assert_eq!(
                boot_action_with(checkpoint, BootDirective::ForceIdentify, now),
                DiscardAndIdentify,
                "armed {state}"
            );
        }
        assert_eq!(BootDirective::default(), BootDirective::None);
    }

    #[test]
    fn sequence_gaps_count_missed_dispatches_only_in_the_same_session() {
        let session = saved();
        assert_eq!(missed_gap(Some(&session), "mock-session", 43), 0);
        assert_eq!(missed_gap(Some(&session), "mock-session", 42), 0);
        assert_eq!(missed_gap(Some(&session), "mock-session", 41), 0);
        assert_eq!(missed_gap(Some(&session), "mock-session", 45), 2);
        assert_eq!(missed_gap(Some(&session), "new-session", 1), 0);
        assert_eq!(missed_gap(None, "mock-session", 7), 0);
        let saturated = GatewaySession {
            sequence: u64::MAX,
            ..saved()
        };
        assert_eq!(missed_gap(Some(&saturated), "mock-session", u64::MAX), 0);
    }

    #[test]
    fn invalid_session_only_falls_back_when_not_resumable() {
        assert!(invalidates_session(false));
        assert!(!invalidates_session(true));
    }
}
