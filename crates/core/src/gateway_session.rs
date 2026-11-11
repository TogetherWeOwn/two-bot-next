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

// Twilight 0.17.1 appends this query before tokio-websockets parses an HTTP
// URI (http 1.x caps it at u16::MAX - 1 bytes). zstd uses the same suffix length;
// reserving compression space is also safe for builds without compression.
const RESUME_CONNECT_SUFFIX: &str = "/?v=10&encoding=json&compress=zlib-stream";
const MAX_RESUME_URL_LEN: usize = (u16::MAX - 1) as usize - RESUME_CONNECT_SUFFIX.len();

/// Persisted endpoints must use Discord's secure gateway domain. Reject raw
/// authority forms that URL parsing would otherwise normalize before checking,
/// and require the original string to be usable by the connection URI parser.
pub fn is_valid_resume_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("wss://") else {
        return false;
    };
    if value.len() > MAX_RESUME_URL_LEN
        || !value.is_ascii()
        || value
            .bytes()
            .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\')
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains(['@', '%']) {
        return false;
    }
    if format!("{value}{RESUME_CONNECT_SUFFIX}")
        .parse::<http::Uri>()
        .is_err()
    {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "wss"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(|host| {
            host == "discord.gg"
                || host
                    .strip_suffix(".discord.gg")
                    .is_some_and(|prefix| !prefix.is_empty())
        })
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
    fn resume_url_policy_accepts_discord_secure_endpoints() {
        for resume_url in [
            "wss://discord.gg",
            "wss://gateway.discord.gg/",
            "wss://us-east.gateway.discord.gg/?v=10&encoding=json",
            "wss://GATEWAY.DISCORD.GG:443/",
        ] {
            assert!(is_valid_resume_url(resume_url), "{resume_url}");
        }
    }

    #[test]
    fn resume_url_policy_discards_other_endpoints() {
        for resume_url in [
            "",
            "not a URL",
            "wss://",
            "wss://[invalid",
            "ws://gateway.discord.gg",
            "https://gateway.discord.gg",
            "wss:gateway.discord.gg",
            "wss:///gateway.discord.gg",
            "wss://discord.gg.example.com",
            "wss://notdiscord.gg",
            "wss://gateway.example.com/discord.gg",
            "wss://127.0.0.1",
            "wss://[::1]",
            "wss://.discord.gg",
            "wss://gateway.discord.gg.",
            "wss://gateway.discord.gg:444",
            "wss://gateway.discord.gg:0",
            "wss://gateway.discord.gg:invalid",
            "wss://user@gateway.discord.gg",
            "wss://user:password@gateway.discord.gg",
            "wss://@gateway.discord.gg",
            "wss://gateway.discord.gg@example.com",
            "wss://gateway.discord.gg\\@example.com",
            "wss://%67ateway.discord.gg",
            "wss://gateway。discord.gg",
            " wss://gateway.discord.gg",
            "wss://gateway.discord.gg\n",
            "wss://gate\tway.discord.gg",
            "wss://gateway.discord.gg/<",
            "wss://gateway.discord.gg/>",
        ] {
            assert!(!is_valid_resume_url(resume_url), "{resume_url:?}");
        }
    }

    #[test]
    fn resume_url_policy_leaves_room_for_twilight_connection_suffix() {
        let prefix = "wss://gateway.discord.gg/";
        let max = (u16::MAX - 1) as usize - "/?v=10&encoding=json&compress=zlib-stream".len();
        let mut resume_url = format!("{prefix}{}", "a".repeat(max - prefix.len()));
        assert!(is_valid_resume_url(&resume_url));
        for compression in ["", "&compress=zlib-stream", "&compress=zstd-stream"] {
            assert!(format!("{resume_url}/?v=10&encoding=json{compression}")
                .parse::<http::Uri>()
                .is_ok());
        }
        resume_url.push('a');
        assert!(!is_valid_resume_url(&resume_url));
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
