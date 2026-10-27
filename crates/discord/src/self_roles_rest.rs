//! Self-role REST operations on the shared executor, never a private client.
//!
//! The runtime owns event/panel claims, renewal, intent, compensation and audit.
//! A role step reserves the shared paced lane, checks ownership immediately
//! before sending, and checks it again after the single bounded exchange. No
//! retry can escape those fences. An accepted exchange with lost ownership is
//! still an observed effect, not success of the enclosing self-role operation.

use super::{ActionExecutor, RawResponse, MODERATION_TIMEOUT_MS};
use crate::ratelimit_guard::GuardError;
use serde::{de::DeserializeOwned, Deserialize};
use std::{collections::HashSet, future::Future, time::Duration};
use twilight_http::request::{Request, TryIntoRequest};
use twilight_model::id::{marker::GenericMarker, Id};
use two_bot_core::self_roles::{
    is_snowflake, validate_self_role_dispatch, ChannelOverwrite, ChannelSnapshot, DispatchCheck,
    DispatchFailure, DispatchRole, SelfRolePanel,
};

/// Scalar errors only: neither provider bodies nor transport detail reach logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelfRoleRestError {
    #[error("invalid self-role Discord id")]
    InvalidId,
    #[error("self-role snapshot could not be verified")]
    Snapshot,
    #[error("self-role claim is no longer owned")]
    StaleClaim,
    #[error("Discord refused the self-role request ({0})")]
    Rejected(u16),
    #[error("Discord rate-limited the self-role request")]
    RateLimited,
    #[error("self-role Discord exchange has an uncertain result")]
    Ambiguous,
}

/// Even when the post-call fence fails, retain the exchange result for auditing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleExchange {
    pub result: Result<(), SelfRoleRestError>,
    pub owned_after: bool,
    /// False on timeout/transport loss: remote work may still finish later.
    /// A subsequent member snapshot cannot establish exchange completion.
    pub response_received: bool,
    /// Final received status, independent of result mapping or post-call ownership.
    /// None means transport/timeout uncertainty, never definite no-send.
    pub response_status: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct SelfRoleSnapshot {
    pub member_role_ids: HashSet<String>,
    pub member_is_bot: bool,
    pub roles: Vec<DispatchRole>,
    pub channels: Vec<ChannelSnapshot>,
    pub bot_has_manage_roles: bool,
    pub bot_highest_position: i64,
}

impl SelfRoleSnapshot {
    #[must_use]
    pub fn validate(
        &self,
        guild_id: &str,
        panel: &SelfRolePanel,
        role_ids: &[String],
    ) -> Option<DispatchFailure> {
        validate_self_role_dispatch(&DispatchCheck {
            guild_id,
            panel,
            role_ids,
            roles: &self.roles,
            channels: &self.channels,
            bot_has_manage_roles: self.bot_has_manage_roles,
            bot_highest_position: self.bot_highest_position,
        })
    }
}

#[derive(Deserialize)]
struct User {
    id: String,
    #[serde(default)]
    bot: bool,
}
#[derive(Deserialize)]
struct Member {
    user: User,
    roles: Vec<String>,
}
#[derive(Deserialize)]
struct Role {
    id: String,
    permissions: String,
    color: u32,
    managed: bool,
    position: i64,
}
#[derive(Deserialize)]
struct Overwrite {
    id: String,
    #[serde(rename = "type")]
    kind: u8,
    allow: String,
    deny: String,
}
#[derive(Deserialize)]
struct Channel {
    id: String,
    guild_id: String,
    name: Option<String>,
    permission_overwrites: Vec<Overwrite>,
}
#[derive(Deserialize)]
struct Message {
    id: String,
    channel_id: String,
}

fn numeric_id(value: &str) -> Result<Id<GenericMarker>, SelfRoleRestError> {
    if !is_snowflake(value) {
        return Err(SelfRoleRestError::InvalidId);
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or(SelfRoleRestError::InvalidId)
}

fn mask(value: &str) -> Result<u64, SelfRoleRestError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SelfRoleRestError::Snapshot);
    }
    value.parse().map_err(|_| SelfRoleRestError::Snapshot)
}

fn status(response: &RawResponse) -> Result<(), SelfRoleRestError> {
    status_code(response.status)
}

fn status_code(code: u16) -> Result<(), SelfRoleRestError> {
    match code {
        200..=299 => Ok(()),
        429 => Err(SelfRoleRestError::RateLimited),
        400..=499 => Err(SelfRoleRestError::Rejected(code)),
        _ => Err(SelfRoleRestError::Ambiguous),
    }
}

/// A local guard refusal never reached the wire, so it is provably no-send.
/// Transient restrictions defer like rate limiting; a dead token is uncertain
/// rather than a rejection with a status code.
fn guard_error(error: GuardError) -> SelfRoleRestError {
    match error {
        GuardError::TokenInvalid => SelfRoleRestError::Ambiguous,
        GuardError::CircuitOpen | GuardError::AdmissionTimeout | GuardError::GlobalPaused => {
            SelfRoleRestError::RateLimited
        }
    }
}

impl ActionExecutor {
    async fn self_role_read<T: DeserializeOwned>(
        &self,
        builder: impl TryIntoRequest,
    ) -> Result<T, SelfRoleRestError> {
        let request = builder
            .try_into_request()
            .map_err(|_| SelfRoleRestError::InvalidId)?;
        let (_lane, response) = self.self_role_read_response(&request).await?;
        serde_json::from_slice(&response.body).map_err(|_| SelfRoleRestError::Snapshot)
    }

    // Share acquisition across decoder types, but retain the reservation in the
    // caller until decoding finishes and the response has been dropped.
    #[inline(never)]
    async fn self_role_read_response(
        &self,
        request: &Request,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, tokio::time::Instant>,
            RawResponse,
        ),
        SelfRoleRestError,
    > {
        let mut lane = self.paced_lane(false).await.map_err(guard_error)?;
        self.stamp_paced_lane(&mut lane, false);
        // `send_admitted_within` skips lane locking: the reservation above is
        // the pacing. The wire budget lives inside the transport — where a
        // stalled GET releases its durable permit — so no outer timeout may
        // wrap this call: an outer abort would drop the future with the
        // permit taken and orphan the lane. It still collects the bounded
        // body, so a truncated member document stays `Ambiguous`, never a
        // parsed partial snapshot.
        let (response, _) = self
            .send_admitted_within(request, Duration::from_millis(MODERATION_TIMEOUT_MS))
            .await
            .map_err(|_| SelfRoleRestError::Ambiguous)?;
        status(&response)?;
        Ok((lane, response))
    }

    /// Reaction removes carry no member, and adds may carry stale member data.
    /// Fetch the configured message through the shared REST seam for either
    /// partial path; do not infer its identity from an unfetched cache entry.
    pub async fn fetch_self_role_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), SelfRoleRestError> {
        let message: Message = self
            .self_role_read(self.inner.factory.message(
                numeric_id(channel_id)?.cast(),
                numeric_id(message_id)?.cast(),
            ))
            .await?;
        if message.id != message_id || message.channel_id != channel_id {
            return Err(SelfRoleRestError::Snapshot);
        }
        Ok(())
    }

    /// Force-fetch all policy and member data AFTER lane acquisition, including
    /// channels for overwrite safety. No gateway cache or caller permission mask
    /// is authoritative. Unknown/duplicate IDs and partial policy fail closed.
    pub async fn fetch_self_role_snapshot(
        &self,
        guild_id: &str,
        member_id: &str,
        bot_id: &str,
    ) -> Result<SelfRoleSnapshot, SelfRoleRestError> {
        let guild = numeric_id(guild_id)?.cast();
        let member: Member = self
            .self_role_read(
                self.inner
                    .factory
                    .guild_member(guild, numeric_id(member_id)?.cast()),
            )
            .await?;
        let bot: Member = self
            .self_role_read(
                self.inner
                    .factory
                    .guild_member(guild, numeric_id(bot_id)?.cast()),
            )
            .await?;
        if member.user.id != member_id || bot.user.id != bot_id || !bot.user.bot {
            return Err(SelfRoleRestError::Snapshot);
        }
        let roles: Vec<Role> = self.self_role_read(self.inner.factory.roles(guild)).await?;
        let mut ids = HashSet::new();
        let mut resolved = Vec::with_capacity(roles.len());
        for role in roles {
            numeric_id(&role.id).map_err(|_| SelfRoleRestError::Snapshot)?;
            if !ids.insert(role.id.clone()) || role.position < 0 {
                return Err(SelfRoleRestError::Snapshot);
            }
            resolved.push(DispatchRole {
                id: role.id,
                permissions: mask(&role.permissions)?,
                color: role.color,
                managed: role.managed,
                position: role.position,
            });
        }
        if !ids.contains(guild_id)
            || member
                .roles
                .iter()
                .chain(&bot.roles)
                .any(|id| !ids.contains(id))
        {
            return Err(SelfRoleRestError::Snapshot);
        }
        let mut permissions = 0;
        let mut highest = 0;
        for role in &resolved {
            if role.id == guild_id || bot.roles.contains(&role.id) {
                permissions |= role.permissions;
                highest = highest.max(role.position);
            }
        }
        let channels: Vec<Channel> = self
            .self_role_read(self.inner.factory.guild_channels(guild))
            .await?;
        let mut seen = HashSet::new();
        let mut channel_snapshots = Vec::with_capacity(channels.len());
        for channel in channels {
            numeric_id(&channel.id).map_err(|_| SelfRoleRestError::Snapshot)?;
            if channel.guild_id != guild_id || !seen.insert(channel.id.clone()) {
                return Err(SelfRoleRestError::Snapshot);
            }
            let mut overwrite_ids = HashSet::new();
            let mut overwrites = Vec::with_capacity(channel.permission_overwrites.len());
            for overwrite in channel.permission_overwrites {
                numeric_id(&overwrite.id).map_err(|_| SelfRoleRestError::Snapshot)?;
                if overwrite.kind > 1
                    || !overwrite_ids.insert((overwrite.id.clone(), overwrite.kind))
                {
                    return Err(SelfRoleRestError::Snapshot);
                }
                overwrites.push(ChannelOverwrite {
                    id: overwrite.id,
                    kind: overwrite.kind,
                    allow: mask(&overwrite.allow)?,
                    deny: mask(&overwrite.deny)?,
                });
            }
            channel_snapshots.push(ChannelSnapshot {
                id: channel.id,
                name: channel.name,
                overwrites,
            });
        }
        Ok(SelfRoleSnapshot {
            member_role_ids: member.roles.into_iter().collect(),
            member_is_bot: member.user.bot,
            roles: resolved,
            channels: channel_snapshots,
            bot_has_manage_roles: permissions & ((1 << 28) | (1 << 3)) != 0,
            bot_highest_position: highest,
        })
    }

    /// One singular role operation. The caller passes a freshly validated plan
    /// and a DB-backed check of BOTH claims. Reservation includes the late check
    /// and exchange, preventing a request from sitting unfenced behind pacing.
    /// Renewal must run concurrently in the runtime; false/error stops the send.
    pub async fn self_role_step<F, Fut>(
        &self,
        guild_id: &str,
        member_id: &str,
        role_id: &str,
        add: bool,
        owns: F,
    ) -> Result<RoleExchange, SelfRoleRestError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<bool, SelfRoleRestError>>,
    {
        self.self_role_step_journaled(guild_id, member_id, role_id, add, owns, || async { Ok(()) })
            .await
    }

    /// The runtime journals send intent inside the shared paced reservation.
    /// A failed checkpoint prevents the send; ownership is rechecked after the
    /// database wait. Cancellation after the checkpoint leaves unresolved intent
    /// for authoritative recovery, never an invented successful exchange.
    pub async fn self_role_step_journaled<F, Fut, J, Journal>(
        &self,
        guild_id: &str,
        member_id: &str,
        role_id: &str,
        add: bool,
        owns: F,
        journal: J,
    ) -> Result<RoleExchange, SelfRoleRestError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<bool, SelfRoleRestError>>,
        J: FnOnce() -> Journal,
        Journal: Future<Output = Result<(), SelfRoleRestError>>,
    {
        let guild = numeric_id(guild_id)?.cast();
        let member = numeric_id(member_id)?.cast();
        let role = numeric_id(role_id)?.cast();
        if role_id == guild_id {
            return Err(SelfRoleRestError::InvalidId);
        }
        let request = if add {
            self.inner
                .factory
                .add_guild_member_role(guild, member, role)
                .try_into_request()
        } else {
            self.inner
                .factory
                .remove_guild_member_role(guild, member, role)
                .try_into_request()
        }
        .map_err(|_| SelfRoleRestError::InvalidId)?;
        let mut lane = self.paced_lane(false).await.map_err(guard_error)?;
        if !owns().await? {
            return Err(SelfRoleRestError::StaleClaim);
        }
        journal().await?;
        if !owns().await? {
            return Err(SelfRoleRestError::StaleClaim);
        }
        // A guard closed during the DB waits refuses instead of sleeping
        // after the fresh fence (kick pattern): never send unfenced.
        if let Err(error) = self.inner.transport.guard.check_now(false) {
            return Err(guard_error(error));
        }
        self.stamp_paced_lane(&mut lane, false);
        let (result, response_status) =
            match tokio::time::timeout(self.inner.moderation_timeout, self.send_status(&request))
                .await
            {
                Ok(Ok(204)) => (Ok(()), Some(204)),
                Ok(Ok(code)) => (
                    status_code(code).and(Err(SelfRoleRestError::Ambiguous)),
                    Some(code),
                ),
                _ => (Err(SelfRoleRestError::Ambiguous), None),
            };
        let owned_after = owns().await.unwrap_or(false);
        Ok(RoleExchange {
            result,
            owned_after,
            response_received: response_status.is_some(),
            response_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_fixture(body: &'static str) -> (ActionExecutor, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.expect("read fixture request");
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            }
            assert!(request.starts_with(b"GET /api/v10/channels/1/messages/42 "));
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write fixture response");
        });
        let executor = ActionExecutor::with_proxy("fixture-read-token".into(), Some(origin))
            .expect("loopback executor");
        (executor, server)
    }

    // These probes run synchronously on each test's current-thread runtime.
    thread_local! {
        static DECODING_EXECUTOR: RefCell<Option<ActionExecutor>> = const { RefCell::new(None) };
        static DECODE_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    #[derive(Debug)]
    struct DecodeProbe<const FAIL: bool>;

    impl<'de, const FAIL: bool> Deserialize<'de> for DecodeProbe<FAIL> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            DECODING_EXECUTOR.with(|slot| {
                let executor = slot.borrow();
                let executor = executor.as_ref().expect("decoder probe installed");
                assert!(
                    executor.inner.pace_last_at.try_lock().is_err(),
                    "typed read must retain its pacing reservation during decoding"
                );
            });
            DECODE_CALLS.with(|calls| calls.set(calls.get() + 1));
            let message = Message::deserialize(deserializer)?;
            assert_eq!(message.id, "42");
            assert_eq!(message.channel_id, "1");
            if FAIL {
                Err(serde::de::Error::custom("fixture decode failure"))
            } else {
                Ok(Self)
            }
        }
    }

    async fn assert_typed_read_reservation<const FAIL: bool>() {
        let (executor, server) = read_fixture(r#"{"id":"42","channel_id":"1"}"#).await;
        assert!(executor.inner.pace_last_at.try_lock().is_ok());
        DECODING_EXECUTOR.with(|slot| *slot.borrow_mut() = Some(executor.clone()));
        DECODE_CALLS.with(|calls| calls.set(0));
        let decoded = tokio::time::timeout(
            Duration::from_secs(5),
            executor.self_role_read::<DecodeProbe<FAIL>>(
                executor.inner.factory.message(Id::new(1), Id::new(42)),
            ),
        )
        .await
        .expect("typed read must finish");
        DECODE_CALLS.with(|calls| assert_eq!(calls.get(), 1));
        DECODING_EXECUTOR.with(|slot| *slot.borrow_mut() = None);
        if FAIL {
            assert_eq!(
                decoded.expect_err("probe must reject the document"),
                SelfRoleRestError::Snapshot
            );
        } else {
            decoded.expect("probe must decode the document");
        }
        assert!(executor.inner.pace_last_at.try_lock().is_ok());
        server.await.expect("fixture server");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_read_retains_pacing_reservation_during_successful_decode() {
        assert_typed_read_reservation::<false>().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_read_retains_pacing_reservation_during_failed_decode() {
        assert_typed_read_reservation::<true>().await;
    }

    #[tokio::test]
    async fn shared_read_retains_pacing_reservation_through_decoding() {
        for body in [r#"{"id":"42","channel_id":"1"}"#, "malformed-fixture-body"] {
            let (executor, server) = read_fixture(body).await;
            let request = executor
                .inner
                .factory
                .message(Id::new(1), Id::new(42))
                .try_into_request()
                .expect("fixture request");
            let (lane, response) = tokio::time::timeout(
                Duration::from_secs(5),
                executor.self_role_read_response(&request),
            )
            .await
            .expect("single read must finish")
            .expect("fixture response");
            assert!(executor.inner.pace_last_at.try_lock().is_err());
            let decoded = serde_json::from_slice::<Message>(&response.body);
            if body.starts_with('{') {
                let message = decoded.expect("typed message");
                assert_eq!(message.id, "42");
                assert_eq!(message.channel_id, "1");
            } else {
                assert!(decoded.is_err());
            }
            drop(response);
            assert!(executor.inner.pace_last_at.try_lock().is_err());
            drop(lane);
            assert!(executor.inner.pace_last_at.try_lock().is_ok());
            server.await.expect("fixture server");
        }
    }
}
