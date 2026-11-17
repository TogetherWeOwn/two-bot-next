//! Website member actions, independent of the HTTP receiver.
//!
//! Single-attempt mutations: the caller owns retry/idempotency and action gates.
//! Never format a request builder, provider body, or transport error here: any
//! of them may contain the member's OAuth token.

use super::{ActionExecutor, RawResponse};
use crate::ratelimit_guard::GuardError;
use serde::Deserialize;
use std::time::Duration;
use twilight_http::request::{Request, TryIntoRequest};
use twilight_model::id::{marker::GenericMarker, Id};
use two_bot_core::internal_actions::{
    ActionError, ErrorCode, GuildAddMemberRequest, RoleAssignRequest,
};

#[cfg(feature = "db")]
#[path = "member_store.rs"]
pub mod store;

#[path = "voice_move.rs"]
pub mod voice_move;
pub use voice_move::{VoiceMoveNotSent, VoiceMovePatch, VOICE_MOVE_TIMEOUT_MS};

pub const ADD_MEMBER_TIMEOUT_MS: u64 = 1500;
pub const ROLE_TIMEOUT_MS: u64 = 2000;

/// Keep proof of an unsent mutation until the durable caller disposes its claim.
/// Public callers still receive the legacy scalar-only `ActionError` envelope.
enum MemberError {
    Guard(GuardError),
    Action(ActionError),
}

impl From<ActionError> for MemberError {
    fn from(error: ActionError) -> Self {
        Self::Action(error)
    }
}

impl MemberError {
    fn into_action_error(self) -> ActionError {
        match self {
            Self::Guard(GuardError::TokenInvalid) => ActionError::new(
                ErrorCode::DiscordUnavailable,
                "Discord bot authentication is unavailable",
                "discord_guard_refused",
            ),
            Self::Guard(_) => ActionError::new(
                ErrorCode::RateLimited,
                "Discord request paused locally before dispatch",
                "discord_guard_refused",
            )
            .with_retry_after(1),
            Self::Action(error) => error,
        }
    }
}

/// Exactly the legacy result object, without a provider response or secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberOutcome {
    Added,
    AlreadyMember,
    Assigned,
    AlreadyHeld,
}

impl MemberOutcome {
    #[must_use]
    pub fn result(self) -> serde_json::Value {
        serde_json::json!({"outcome": self})
    }

    /// Legacy successBody insertion order is part of the website wire contract.
    #[must_use]
    pub fn success_body(self, request_id: &str) -> serde_json::Value {
        serde_json::json!({"ok": true, "result": self.result(), "request_id": request_id})
    }
}

/// Legacy internal/discordActions.ts throwForStatus, not moderation's body-based
/// retry parser. Only safe status/header scalars reach the public error.
pub fn member_status_error(response: &RawResponse) -> Option<ActionError> {
    let status = response.status;
    if (200..300).contains(&status) {
        return None;
    }
    // Raw Hyper never follows redirects. Do not persist an unfollowed exchange
    // as success or forward credentials to the Location target.
    if status < 400 {
        return Some(ActionError::new(
            ErrorCode::DiscordUnavailable,
            format!("Discord returned unexpected status {status}"),
            "discord_unexpected_status",
        ));
    }
    if status == 429 {
        let retry = response
            .retry_after_header
            .as_deref()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .unwrap_or(1.0)
            .ceil()
            .max(1.0) as u64;
        return Some(
            ActionError::new(
                ErrorCode::RateLimited,
                "Discord rate-limited this request",
                "discord_rate_limited",
            )
            .with_retry_after(retry),
        );
    }
    if status >= 500 {
        return Some(ActionError::new(
            ErrorCode::DiscordUnavailable,
            format!("Discord returned {status}"),
            "discord_5xx",
        ));
    }
    let hint = if status == 403 {
        " (check the bot has the permission, and that its highest role is above the target role)"
    } else {
        ""
    };
    Some(ActionError::new(
        ErrorCode::DiscordRejected,
        format!("Discord refused the request with {status}{hint}"),
        format!("discord_{status}"),
    ))
}

fn numeric_id(value: &str) -> Result<Id<GenericMarker>, ActionError> {
    value
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or_else(|| {
            ActionError::new(ErrorCode::Malformed, "Invalid Discord ID", "bad_discord_id")
        })
}

fn unreadable() -> ActionError {
    ActionError::new(
        ErrorCode::DiscordUnavailable,
        "Discord returned an unreadable role snapshot",
        "role_fetch_failed",
    )
}

#[derive(Deserialize)]
struct RoleSnapshot {
    id: String,
    position: i64,
    managed: bool,
}

impl ActionExecutor {
    // Only building the request depends on its Twilight builder type. Keep the
    // bounded exchange and refusal handling in one concrete async state machine.
    #[inline(never)]
    async fn member_request(
        &self,
        request: Result<Request, twilight_http::Error>,
        timeout_ms: u64,
    ) -> Result<RawResponse, MemberError> {
        let request = request.map_err(|_| {
            ActionError::new(
                ErrorCode::Malformed,
                "Invalid Discord request",
                "discord_request_invalid",
            )
        })?;
        let (response, _) = self
            .send_with_timeout_for(&request, None, Duration::from_millis(timeout_ms))
            .await
            .map_err(|error| match error {
                super::DiscordError::Guard(error) => MemberError::Guard(error),
                super::DiscordError::Timeout => MemberError::Action(ActionError::new(
                    ErrorCode::UpstreamTimeout,
                    format!("Discord did not answer in {timeout_ms}ms"),
                    "discord_timeout",
                )),
                _ => MemberError::Action(ActionError::new(
                    ErrorCode::DiscordUnavailable,
                    "Discord was unreachable",
                    "discord_unreachable",
                )),
            })?;
        if let Some(error) = member_status_error(&response) {
            tracing::warn!(code = error.code.as_str(), reason = %error.log_reason,
                "internal member action refused");
            return Err(error.into());
        }
        Ok(response)
    }

    async fn internal_member_roles(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> Result<Option<Vec<String>>, MemberError> {
        let response = self
            .member_request(
                self.inner
                    .factory
                    .guild_member(numeric_id(guild_id)?.cast(), numeric_id(user_id)?.cast())
                    .try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        #[derive(Deserialize)]
        struct Member {
            roles: Vec<String>,
        }
        Ok(serde_json::from_slice::<Member>(&response.body)
            .ok()
            .map(|m| m.roles))
    }

    /// Send one bounded voice move. The receiver owns authorization and fencing.
    pub async fn move_voice_member(
        &self,
        guild_id: &str,
        member_id: &str,
        channel_id: &str,
    ) -> Result<VoiceMovePatch, ActionError> {
        let request =
            voice_move::prepare_voice_move(&self.inner.factory, guild_id, member_id, channel_id)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(VOICE_MOVE_TIMEOUT_MS);
        match tokio::time::timeout_at(deadline, self.admit(&request, Some(false))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return Ok(VoiceMovePatch::NotSent(VoiceMoveNotSent::Guard)),
        }
        self.count();
        let pending = match self
            .inner
            .transport
            .send_strict_request_headers(&request, Some(deadline))
            .await
        {
            Ok(pending) => pending,
            Err(super::StrictTransportError::Admission(
                two_bot_core::send_admission::AdmissionError::Blocked,
            )) => return Ok(VoiceMovePatch::NotSent(VoiceMoveNotSent::Blocked)),
            Err(super::StrictTransportError::Admission(
                two_bot_core::send_admission::AdmissionError::Storage,
            )) => return Ok(VoiceMovePatch::NotSent(VoiceMoveNotSent::Storage)),
            Err(_) => return Ok(VoiceMovePatch::Unavailable),
        };
        if let Some(refusal) = voice_move::header_refusal(pending.status) {
            let _ = tokio::time::timeout_at(deadline, pending.settle_status()).await;
            return Ok(refusal);
        }
        match pending.status {
            429 | 200..=299 => {
                let status = pending.status;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let (mut response, _) = match tokio::time::timeout_at(
                    deadline,
                    pending.collect_with(remaining),
                )
                .await
                {
                    Ok(Ok(response)) => response,
                    _ if status == 429 => {
                        return Ok(VoiceMovePatch::Limited {
                            retry_after_ms: None,
                        })
                    }
                    _ => return Ok(VoiceMovePatch::Unavailable),
                };
                if response.status == 429 {
                    let retry_after_ms = voice_move::retry_delay_ms(
                        response
                            .retry_after_header
                            .as_deref()
                            .and_then(|s| s.parse().ok()),
                        response.body_retry_after_secs(),
                    );
                    Ok(VoiceMovePatch::Limited { retry_after_ms })
                } else {
                    match tokio::time::timeout_at(deadline, response.complete_confirmed()).await {
                        Ok(Ok(())) => Ok(VoiceMovePatch::Moved),
                        _ => Ok(VoiceMovePatch::Unavailable),
                    }
                }
            }
            _ => {
                let _ = tokio::time::timeout_at(deadline, pending.settle_status()).await;
                Ok(VoiceMovePatch::Unavailable)
            }
        }
    }

    /// PUT /guilds/{g}/members/{u}. The token is a function argument and wire
    /// body only. Source: <https://docs.discord.com/developers/resources/guild#add-guild-member>
    pub async fn add_internal_member(
        &self,
        guild_id: &str,
        request: &GuildAddMemberRequest<'_>,
        access_token: &str,
    ) -> Result<MemberOutcome, ActionError> {
        self.add_internal_member_once(guild_id, request, access_token)
            .await
            .map_err(MemberError::into_action_error)
    }

    async fn add_internal_member_once(
        &self,
        guild_id: &str,
        request: &GuildAddMemberRequest<'_>,
        access_token: &str,
    ) -> Result<MemberOutcome, MemberError> {
        if access_token.is_empty() {
            return Err(ActionError::new(
                ErrorCode::Malformed,
                "\"access_token\" must be a non-empty string",
                "missing_access_token",
            )
            .into());
        }
        let mut response = self
            .member_request(
                self.inner
                    .factory
                    .add_guild_member(
                        numeric_id(guild_id)?.cast(),
                        numeric_id(request.discord_id())?.cast(),
                        access_token,
                    )
                    .try_into_request(),
                ADD_MEMBER_TIMEOUT_MS,
            )
            .await?;
        let outcome = match response.status {
            201 => MemberOutcome::Added,
            200 | 204 => MemberOutcome::AlreadyMember,
            _ => return Err(unreadable().into()),
        };
        // This endpoint's accepted status distinguishes added/already present;
        // no message/resource id is consumed by the caller.
        response.complete().await;
        Ok(outcome)
    }

    /// A resolved allowlisted role, with an authoritative hierarchy read before
    /// mutation. Source: <https://docs.discord.com/developers/topics/permissions#permission-hierarchy>
    pub async fn assign_internal_role(
        &self,
        guild_id: &str,
        bot_user_id: &str,
        request: &RoleAssignRequest<'_>,
    ) -> Result<MemberOutcome, ActionError> {
        self.assign_internal_role_once(guild_id, bot_user_id, request)
            .await
            .map_err(MemberError::into_action_error)
    }

    async fn assign_internal_role_once(
        &self,
        guild_id: &str,
        bot_user_id: &str,
        request: &RoleAssignRequest<'_>,
    ) -> Result<MemberOutcome, MemberError> {
        let guild = numeric_id(guild_id)?.cast();
        let user = numeric_id(request.discord_id())?.cast();
        let role = numeric_id(request.role_id())?.cast();
        numeric_id(bot_user_id)?;
        // Legacy: failure to read the target is not proof of absence, but a
        // redundant role PUT is naturally idempotent. A 429 must stop all further
        // REST calls during cooldown. Policy reads still fail closed.
        match self
            .internal_member_roles(guild_id, request.discord_id())
            .await
        {
            Ok(Some(held)) if held.iter().any(|id| id == request.role_id()) => {
                return Ok(MemberOutcome::AlreadyHeld);
            }
            Err(error @ MemberError::Guard(_)) => return Err(error),
            Err(MemberError::Action(error)) if error.code == ErrorCode::RateLimited => {
                return Err(error.into());
            }
            _ => {}
        }
        let bot_roles = self
            .internal_member_roles(guild_id, bot_user_id)
            .await?
            .ok_or_else(unreadable)?;
        let response = self
            .member_request(
                self.inner.factory.roles(guild).try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        let roles: Vec<RoleSnapshot> =
            serde_json::from_slice(&response.body).map_err(|_| unreadable())?;
        // Reject malformed/duplicate snapshot IDs, including unknown bot roles.
        let mut ids = std::collections::HashSet::new();
        for snapshot in &roles {
            numeric_id(&snapshot.id).map_err(|_| unreadable())?;
            if !ids.insert(snapshot.id.as_str()) {
                return Err(unreadable().into());
            }
        }
        if !ids.contains(guild_id) || bot_roles.iter().any(|id| !ids.contains(id.as_str())) {
            return Err(unreadable().into());
        }
        let target = roles
            .iter()
            .find(|r| r.id == request.role_id())
            .ok_or_else(unreadable)?;
        let highest = roles
            .iter()
            .filter(|r| r.id == guild_id || bot_roles.contains(&r.id))
            .map(|r| r.position)
            .max()
            .ok_or_else(unreadable)?;
        if target.managed || target.id == guild_id || target.position >= highest {
            return Err(member_status_error(&RawResponse {
                status: 403,
                retry_after_header: None,
                body: vec![],
                completion: None,
            })
            .expect("403 is a refusal")
            .into());
        }
        let mut response = self
            .member_request(
                self.inner
                    .factory
                    .add_guild_member_role(guild, user, role)
                    .try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        if response.status != 204 {
            return Err(unreadable().into());
        }
        response.complete().await;
        Ok(MemberOutcome::Assigned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use two_bot_core::send_admission::{
        AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion,
        SendCooldown, TokenKey,
    };

    use crate::strict_transport_fixture::{read_request as read_move_request, Admission};

    // This deliberately stalled fake does not model durable fencing.
    #[derive(Debug)]
    struct FixtureAdmission {
        key: TokenKey,
        held: AtomicBool,
    }
    struct FixtureCompletion;

    impl SendCompletion for FixtureCompletion {
        fn complete(
            self: Box<Self>,
            _: Option<SendCooldown>,
        ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
            Box::pin(std::future::pending())
        }
    }

    impl SendAdmission for FixtureAdmission {
        fn token_key(&self) -> &TokenKey {
            &self.key
        }

        fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
            Box::pin(async { Err(AdmissionError::Storage) })
        }

        fn admit_strict(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
            Box::pin(async move {
                if self.held.swap(true, Ordering::SeqCst) {
                    return Err(AdmissionError::Blocked);
                }
                Ok(AdmissionPermit::new(Box::new(FixtureCompletion)))
            })
        }
    }

    fn fixture_executor_with_refusal(
        address: std::net::SocketAddr,
        refusal: Option<AdmissionError>,
        stall: bool,
    ) -> ActionExecutor {
        let token = "fixture-bot-token";
        let admission: Arc<dyn SendAdmission> = if stall {
            Arc::new(FixtureAdmission {
                key: TokenKey::for_bot_token(token).unwrap(),
                held: AtomicBool::new(false),
            })
        } else {
            Arc::new(Admission::new(
                token,
                Some(AdmissionError::Blocked),
                refusal,
            ))
        };
        ActionExecutor::with_admission(token.into(), Some(format!("http://{address}")), admission)
            .unwrap()
    }

    fn fixture_executor(address: std::net::SocketAddr) -> ActionExecutor {
        fixture_executor_with_refusal(address, None, false)
    }

    async fn move_response(response: &'static [u8]) -> VoiceMovePatch {
        move_response_with_stall(response, false).await
    }

    async fn move_response_with_stall(response: &'static [u8], stall: bool) -> VoiceMovePatch {
        use tokio::io::AsyncWriteExt;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_move_request(&mut stream).await;
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("PATCH /api/v10/guilds/1/members/2 "));
            assert!(request.contains("\"channel_id\":\"3\""));
            stream.write_all(response).await.unwrap();
            if stall {
                let _ = wait.await;
            }
        });
        let executor = fixture_executor(address);
        let started = tokio::time::Instant::now();
        let result = executor.move_voice_member("1", "2", "3").await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(VOICE_MOVE_TIMEOUT_MS + 1000));
        let _ = release.send(());
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn move_executor_classifies_receipts_and_broken_bodies() {
        assert_eq!(
            move_response(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await,
            VoiceMovePatch::Moved
        );
        for (response, expected) in [
            (&b"HTTP/1.1 400 Bad Request\r\nContent-Length: 10\r\n\r\n"[..], VoiceMovePatch::Stale),
            (&b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\n\r\n"[..], VoiceMovePatch::Denied),
            (&b"HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\n\r\n"[..], VoiceMovePatch::Stale),
            (&b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n"[..], VoiceMovePatch::Unavailable),
            (&b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n"[..], VoiceMovePatch::Unavailable),
            (&b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1.2\r\nContent-Length: 10\r\n\r\n"[..], VoiceMovePatch::Limited { retry_after_ms: Some(1200) }),
        ] {
            assert_eq!(move_response(response).await, expected);
        }
    }

    #[tokio::test]
    async fn move_rate_limit_uses_longest_timing_or_indefinite() {
        assert_eq!(
            move_response(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 17\r\n\r\n{\"retry_after\":3}").await,
            VoiceMovePatch::Limited { retry_after_ms: Some(3000) }
        );
        assert_eq!(
            move_response(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n").await,
            VoiceMovePatch::Limited {
                retry_after_ms: None
            }
        );
    }

    #[tokio::test]
    async fn move_requests_share_the_paced_lane() {
        use tokio::io::AsyncWriteExt;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_move_request(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
        });
        let mut executor = fixture_executor(address);
        let (probe, mut admissions) = tokio::sync::mpsc::unbounded_channel();
        Arc::get_mut(&mut executor.inner).unwrap().pacing_probe = Some(probe);
        assert_eq!(
            executor.move_voice_member("1", "2", "3").await.unwrap(),
            VoiceMovePatch::Moved
        );
        assert_eq!(
            executor.move_voice_member("1", "2", "3").await.unwrap(),
            VoiceMovePatch::Moved
        );
        let first = admissions.recv().await.unwrap();
        let second = admissions.recv().await.unwrap();
        assert!(matches!(first.lane, super::super::PacingLane::Shared));
        assert!(matches!(second.lane, super::super::PacingLane::Shared));
        assert!(
            second.at.duration_since(first.at)
                >= Duration::from_millis(super::super::PACE_INTERVAL_MS)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_bodies_respect_deadline_and_header_verdicts() {
        for (wire, expected) in [
            (
                &b"HTTP/1.1 400 Bad Request\r\nContent-Length: 10\r\n\r\n"[..],
                VoiceMovePatch::Stale,
            ),
            (
                &b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\n\r\n"[..],
                VoiceMovePatch::Denied,
            ),
            (
                &b"HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\n\r\n"[..],
                VoiceMovePatch::Stale,
            ),
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n"[..],
                VoiceMovePatch::Unavailable,
            ),
            (
                &b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 10\r\n\r\n"[..],
                VoiceMovePatch::Limited {
                    retry_after_ms: None,
                },
            ),
        ] {
            assert_eq!(move_response_with_stall(wire, true).await, expected);
        }
    }

    #[tokio::test]
    async fn strict_admission_failures_are_typed_and_never_send() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for (refusal, expected) in [
            (AdmissionError::Blocked, VoiceMoveNotSent::Blocked),
            (AdmissionError::Storage, VoiceMoveNotSent::Storage),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let executor =
                fixture_executor_with_refusal(listener.local_addr().unwrap(), Some(refusal), false);
            assert_eq!(
                executor.move_voice_member("1", "2", "3").await.unwrap(),
                VoiceMovePatch::NotSent(expected)
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn stalled_settlement_preserves_definite_refusal_and_holds_admission() {
        use tokio::io::AsyncWriteExt;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_move_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
        });
        let executor = fixture_executor_with_refusal(address, None, true);
        let started = tokio::time::Instant::now();
        assert_eq!(
            executor.move_voice_member("1", "2", "3").await.unwrap(),
            VoiceMovePatch::Denied
        );
        assert!(started.elapsed() < Duration::from_millis(VOICE_MOVE_TIMEOUT_MS + 1000));
        assert_eq!(
            executor.move_voice_member("1", "2", "3").await.unwrap(),
            VoiceMovePatch::NotSent(VoiceMoveNotSent::Blocked)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_member_request_keeps_its_redacted_error() {
        let executor = ActionExecutor::with_proxy(
            "fixture-bot-token".into(),
            Some("http://127.0.0.1:1".into()),
        )
        .expect("loopback executor");
        let content = "fixture-private-content".repeat(100);
        let error = executor
            .member_request(
                executor
                    .inner
                    .factory
                    .create_message(Id::new(1))
                    .content(&content)
                    .try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await
            .expect_err("oversized request must fail before dispatch");
        let MemberError::Action(error) = error else {
            panic!("invalid request must not reach admission");
        };
        assert_eq!(error.code, ErrorCode::Malformed);
        assert_eq!(error.message, "Invalid Discord request");
        assert_eq!(error.log_reason, "discord_request_invalid");
        assert!(!format!("{error:?}").contains("fixture"));
    }
}
