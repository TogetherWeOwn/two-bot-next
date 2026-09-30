//! Gateway shard supervisor (S3).
//!
//! Owns the shard lifecycle state the /readyz gate reads and runs the
//! twilight [`Shard`] event loop. Every gateway dispatch goes through the
//! [`Pipeline`]: the cache updates inside `handle()`, so dispatch here is
//! one line plus the `Error` row (parity matrix §3: legacy `client_error`
//! log → `tracing::warn!`).
//!
//! RESUME across Container restarts: twilight parses RESUMED as its own
//! variant and the pipeline drops open voice sessions on both READY (fresh
//! session after re-identify) and RESUMED (TOG-6123), so no outage-inflated
//! durations are reported. Persisting the session bytes (`shard.session()` →
//! Postgres, `ConfigBuilder::session` at boot) is the S5 slice — this module
//! exposes [`session_snapshot`] as the seam: serialize the returned
//! [`Session`] and hand it to S5's store.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Session, Shard, ShardId, StreamExt as _};
use two_bot_core::{
    ComponentStatus, Config, FactsSink, FunnelStore, InviteSnapshotStore, InviteState,
    LevelingHook, NoopFacts, NoopLeveling, Snowflake,
};
use two_bot_discord::{
    gateway_intents, needs_message_content, ChannelClassifier, InviteSource, NoClassification,
    Pipeline,
};
use two_bot_store::{PgFunnelStore, PgInviteSnapshots};

/// Install the process-wide rustls crypto provider (ring) unless one is set.
///
/// Building a [`Shard`] panics without exactly one provider. The binary calls
/// this at startup; tests call it on demand (`install_default` succeeds only
/// once per process, hence the `get_default` guard).
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Supervisor-visible gateway state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayState {
    /// No token configured; the shard will not start.
    Unconfigured,
    /// Token present; the supervisor task is (re)connecting.
    Armed,
    /// Shard connected and identified (constructed by the supervisor,
    /// exercised by the /readyz test).
    Connected,
}

impl GatewayState {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        if config.gateway_configured() {
            Self::Armed
        } else {
            Self::Unconfigured
        }
    }

    #[must_use]
    pub fn status(&self) -> ComponentStatus {
        match self {
            Self::Unconfigured => ComponentStatus::Down,
            Self::Armed => ComponentStatus::Starting,
            Self::Connected => ComponentStatus::Ready,
        }
    }
}

/// Snapshot the shard's resumable session for S5's persistence slice.
///
/// Returns `None` when the shard has no active session (invalidated and not
/// yet reconnected). S5 stores `Session::id` + `Session::sequence` in
/// Postgres and offers them back via `ConfigBuilder::session` at boot.
///
/// Only called by the S5 persistence task (and its test); allow dead code
/// until that slice wires it up.
#[allow(dead_code)]
#[must_use]
pub fn session_snapshot(shard: &Shard) -> Option<Session> {
    shard.session().cloned()
}

/// Resolve the gateway intents from the environment, mirroring legacy
/// `needsMessageContent` (`src/discord/client.ts`): privileged
/// `MESSAGE_CONTENT` only when enabled automod inspects public messages
/// (`TWO_AUTOMOD=1`) or tickets are configured.
pub fn intents_from_env() -> Intents {
    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    let message_content = needs_message_content(
        &var("TWO_AUTOMOD"),
        [
            var("DISCORD_TICKET_CATEGORY_ID").as_str(),
            var("DISCORD_TICKET_STAFF_ROLE_ID").as_str(),
            var("DISCORD_TICKET_PANEL_CHANNEL_ID").as_str(),
        ],
    );
    gateway_intents(message_content)
}

/// Run the shard event loop until the stream ends or fatally closes.
///
/// Dispatches every event to `pipeline`; receive/parse failures log at warn
/// (legacy `Error` → `client_error` row) and the loop continues — a single
/// poisoned dispatch must not kill the funnel. Updates `state` so /readyz
/// tracks the connection.
pub async fn run_shard<S, L, F, I, C, P>(
    mut shard: Shard,
    pipeline: Arc<Pipeline<S, L, F, I, C, P>>,
    state: Arc<RwLock<GatewayState>>,
) where
    S: FunnelStore,
    L: LevelingHook,
    F: FactsSink,
    I: InviteSource,
    C: ChannelClassifier,
    P: InviteSnapshotStore + Send + Sync,
{
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    while let Some(item) = shard.next_event(EventTypeFlags::all()).await {
        match item {
            Ok(event) => {
                if matches!(event, Event::Ready(_)) {
                    *state.write().await = GatewayState::Connected;
                }
                pipeline.handle(&event);
            }
            Err(source) => {
                // Parity matrix §3 `Error` row: legacy logged client_error;
                // here the droppable dispatch is skipped and the loop lives.
                warn!(error = ?source, "gateway dispatch failed; skipping event");
            }
        }
    }
    warn!("gateway shard stream ended; supervisor reports down until restart");
    *state.write().await = GatewayState::Armed;
}

/// Build the supervisor's shard: single-shard deployment (one guild, ADR
/// 0001) over [`intents_from_env`].
///
/// A stored [`Session`] (S5) resumes the previous gateway session instead of
/// a fresh IDENTIFY.
#[must_use]
pub fn build_shard(token: String, intents: Intents, session: Option<Session>) -> Shard {
    use twilight_gateway::ConfigBuilder;
    let mut builder = ConfigBuilder::new(token, intents);
    if let Some(session) = session {
        builder = builder.session(session);
    }
    Shard::with_config(ShardId::ONE, builder.build())
}

/// REST invite counters. A failed or incomplete read keeps the persisted
/// baseline; it must never look like a successful empty guild listing.
pub struct HttpInvites {
    client: twilight_http::Client,
    handle: tokio::runtime::Handle,
}

impl InviteSource for HttpInvites {
    fn current(&self, guild_id: Snowflake) -> Option<Vec<InviteState>> {
        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let invites = self
                        .client
                        .guild_invites(twilight_model::id::Id::new(guild_id))
                        .await
                        .ok()?
                        .model()
                        .await
                        .ok()?;
                    invites
                        .into_iter()
                        .map(|i| {
                            Some(InviteState {
                                code: i.code,
                                uses: i.uses?,
                                inviter_id: i.inviter.map(|u| u.id.get()),
                                channel_id: i.channel.map(|c| c.id.get()),
                            })
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .await
                .ok()
                .flatten();
                if result.is_none() {
                    warn!(
                        guild_id,
                        "invite counter read unavailable; retaining snapshot"
                    );
                }
                result
            })
        })
    }
}

pub type PgPipeline = Pipeline<
    PgFunnelStore,
    NoopLeveling,
    NoopFacts,
    HttpInvites,
    NoClassification,
    PgInviteSnapshots,
>;

/// The runtime never silently falls back to the replay store.
#[must_use]
pub fn build_pipeline(pool: sqlx::PgPool, token: String) -> PgPipeline {
    Pipeline::with_snapshots(
        PgFunnelStore::new(pool.clone()),
        Some(NoopLeveling),
        Some(NoopFacts),
        HttpInvites {
            client: twilight_http::Client::new(token),
            handle: tokio::runtime::Handle::current(),
        },
        NoClassification,
        PgInviteSnapshots::new(pool),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Config {
        Config {
            discord_token: Some("token".to_owned()),
            database_url: None,
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        }
    }

    #[test]
    fn armed_reports_starting_not_ready() {
        let state = GatewayState::new(&configured());
        assert_eq!(state, GatewayState::Armed);
        assert_eq!(state.status(), ComponentStatus::Starting);
    }

    #[test]
    fn unconfigured_reports_down() {
        let state = GatewayState::new(&Config {
            discord_token: None,
            database_url: None,
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        });
        assert_eq!(state.status(), ComponentStatus::Down);
    }

    #[tokio::test]
    async fn fresh_shard_has_no_session_to_persist() {
        ensure_crypto_provider();
        let shard = build_shard("token".to_owned(), Intents::empty(), None);
        assert_eq!(session_snapshot(&shard), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rest_invite_double_distinguishes_missing_counters_from_empty_listing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        ensure_crypto_provider();
        for (status, body, expected_len) in [
            (
                200,
                r#"[{"type":0,"code":"fixture","channel":null,"uses":7}]"#,
                Some(1),
            ),
            (200, r#"[{"type":0,"code":"fixture","channel":null}]"#, None),
            (200, "[]", Some(0)),
            (403, r#"{"message":"fixture denied","code":50013}"#, None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let host = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                }
                assert!(String::from_utf8_lossy(&request).contains("/guilds/123/invites"));
                let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let source = HttpInvites {
                client: twilight_http::Client::builder()
                    .token("fixture".to_owned())
                    .proxy(host, true)
                    .build(),
                handle: tokio::runtime::Handle::current(),
            };
            let result = source.current(123);
            assert_eq!(result.as_ref().map(Vec::len), expected_len);
            if expected_len == Some(1) {
                assert_eq!(result.unwrap()[0].uses, 7);
            }
            server.await.unwrap();
        }
    }
}
