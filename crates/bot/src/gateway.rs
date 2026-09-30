//! Gateway shard supervisor (S3).
//!
//! Owns the shard lifecycle state the /readyz gate reads and runs the
//! twilight [`Shard`] event loop. Every gateway dispatch goes through the
//! [`Pipeline`]: the cache updates inside `handle()`, so dispatch here is
//! one line plus the `Error` row (parity matrix §3: legacy `client_error`
//! log → `tracing::warn!`).
//!
//! RESUME across Container restarts uses the last committed dispatch sequence.
//! The pipeline drops open voice sessions on both READY and RESUMED. Every
//! funnel batch commits with its checkpoint; persistence/dispatch failures
//! stop the runner rather than checkpointing ahead of uncommitted effects.

use std::sync::Arc;

use futures_util::StreamExt as _;
use tokio::sync::RwLock;
use tracing::{info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Message, Session, Shard, ShardId};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{
    boot_action, dispatch_action, invalidates_session, BootAction, DispatchAction, GatewaySession,
};
use two_bot_core::{ComponentStatus, Config, FunnelEvent};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{
    gateway_intents, needs_message_content, NoClassification, NoInvites, Pipeline,
};

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
    /// Missing gateway prerequisites; the shard will not start.
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

/// Resolve the gateway intents from the environment: privileged
/// `MESSAGE_CONTENT` only when enabled automod inspects public messages
/// (`TWO_AUTOMOD=1`), tickets are configured, or custom text commands are
/// explicitly enabled (`TWO_AUTOMATIONS=1` and `TWO_TEXT_COMMANDS=1`).
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
    ) || two_bot_discord::intents::needs_text_command_message_content(
        &var("TWO_AUTOMATIONS"),
        &var("TWO_TEXT_COMMANDS"),
    );
    gateway_intents(message_content)
}

pub type GatewayPipeline = Pipeline<GatewayFunnelBuffer>;

pub async fn load_boot_session(
    store: &GatewaySessionStore,
) -> Result<Option<GatewaySession>, sqlx::Error> {
    let saved = store.load().await?;
    match boot_action(saved.as_ref(), two_bot_core::funnel::now_millis_for_test()) {
        BootAction::Resume => Ok(saved),
        BootAction::DiscardAndIdentify => {
            store.clear().await?;
            Ok(None)
        }
        BootAction::Identify => Ok(None),
    }
}

const CHECKPOINT_IO_MAX: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(serde::Deserialize)]
struct Header {
    op: u8,
    s: Option<u64>,
}

#[derive(serde::Deserialize)]
struct HelloPacket {
    d: Hello,
}

#[derive(serde::Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

/// Bound the entire SQL operation (pool acquire through COMMIT), not each query.
/// Twilight only drives heartbeats while polled, so use at most a quarter of
/// HELLO's interval and fail closed instead of waiting through missed heartbeats.
/// Source: https://docs.rs/tokio/1/tokio/time/fn.timeout.html
async fn checkpoint_io<T>(
    state: &RwLock<GatewayState>,
    deadline: std::time::Duration,
    operation: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    let previous = {
        let mut state = state.write().await;
        let previous = *state;
        *state = GatewayState::Armed;
        previous
    };
    let result = tokio::time::timeout(deadline, operation)
        .await
        .map_err(|_| sqlx::Error::InvalidArgument("gateway checkpoint deadline exceeded".into()))?;
    if result.is_ok() {
        *state.write().await = previous;
    }
    result
}

/// Drive raw packets so even dispatches not mapped by Twilight have a durable
/// sequence. Twilight itself still owns transport, heartbeat and opcode-9
/// fallback. Source: https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Shard.html
pub async fn run_shard(
    mut shard: Shard,
    pipeline: Arc<GatewayPipeline>,
    state: Arc<RwLock<GatewayState>>,
    store: GatewaySessionStore,
) -> Result<(), sqlx::Error> {
    let result = run_loop(&mut shard, &pipeline, &state, &store).await;
    *state.write().await = GatewayState::Armed;
    result
}

async fn run_loop(
    shard: &mut Shard,
    pipeline: &GatewayPipeline,
    state: &RwLock<GatewayState>,
    store: &GatewaySessionStore,
) -> Result<(), sqlx::Error> {
    let mut deadline = CHECKPOINT_IO_MAX;
    let mut committed = checkpoint_io(state, deadline, store.load()).await?;
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    while let Some(item) = shard.next().await {
        let message = match item {
            Ok(message) => message,
            Err(error)
                if matches!(
                    error.kind(),
                    twilight_gateway::error::ReceiveMessageErrorType::Reconnect
                ) =>
            {
                *state.write().await = GatewayState::Armed;
                warn!("gateway reconnect failed; Twilight will retry");
                continue;
            }
            Err(_) => {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway receive failed; checkpoint unchanged".into(),
                ))
            }
        };
        let Message::Text(text) = message else {
            *state.write().await = GatewayState::Armed;
            // Twilight 0.17.1 retains its session on gateway-initiated closes.
            // Discord requires a new session for these two reconnectable codes.
            // Source: https://docs.discord.com/developers/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes
            let rejected = matches!(
                message,
                Message::Close(Some(ref frame)) if matches!(frame.code, 4007 | 4009)
            );
            if rejected || shard.session().is_none() {
                checkpoint_io(state, deadline, store.clear()).await?;
                committed = None;
            }
            if rejected {
                // Shard construction consumed the config's saved session/URL,
                // so this fresh shard IDENTIFYs while retaining intents/queue.
                *shard = Shard::with_config(shard.id(), shard.config().clone());
            }
            continue;
        };
        let header: Header = serde_json::from_str(&text)
            .map_err(|_| sqlx::Error::InvalidArgument("invalid gateway header".into()))?;
        if header.op == 10 {
            let hello: HelloPacket = serde_json::from_str(&text)
                .map_err(|_| sqlx::Error::InvalidArgument("invalid gateway hello".into()))?;
            if hello.d.heartbeat_interval == 0 {
                return Err(sqlx::Error::InvalidArgument(
                    "zero heartbeat interval".into(),
                ));
            }
            deadline = CHECKPOINT_IO_MAX
                .min(std::time::Duration::from_millis(hello.d.heartbeat_interval) / 4);
        }
        if header.op == 9 {
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                sqlx::Error::InvalidArgument("invalid gateway session packet".into())
            })?;
            let resumable = value["d"].as_bool().ok_or_else(|| {
                sqlx::Error::InvalidArgument("invalid gateway session flag".into())
            })?;
            *state.write().await = GatewayState::Armed;
            if invalidates_session(resumable) {
                checkpoint_io(state, deadline, store.clear()).await?;
                committed = None;
            }
        }
        if header.op != 0 {
            continue;
        }
        let sequence = header
            .s
            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing sequence".into()))?;
        let session = session_snapshot(shard)
            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing session".into()))?;
        // Twilight drops resume_url on a failed connect, but retains the session
        // and may successfully RESUME at its bootstrap endpoint. RESUMED carries
        // no new URL: retain READY's committed URL only for this same session.
        let resume_url = shard
            .resume_url()
            .or_else(|| {
                committed
                    .as_ref()
                    .filter(|saved| saved.session_id == session.id())
                    .map(|saved| saved.resume_url.as_str())
            })
            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing resume URL".into()))?;
        let checkpoint = GatewaySession {
            session_id: session.id().to_owned(),
            sequence,
            resume_url: resume_url.to_owned(),
            updated_at_ms: two_bot_core::funnel::now_millis_for_test(),
        };
        if dispatch_action(committed.as_ref(), &checkpoint.session_id, sequence)
            == DispatchAction::Duplicate
        {
            continue;
        }
        let parsed = twilight_gateway::parse(text, EventTypeFlags::all()).map_err(|_| {
            sqlx::Error::InvalidArgument(
                "gateway dispatch parse failed; checkpoint unchanged".into(),
            )
        })?;
        let mut connected = false;
        if let Some(parsed) = parsed {
            let event = Event::from(parsed);
            connected = matches!(event, Event::Ready(_) | Event::Resumed);
            pipeline.handle(&event);
        }
        checkpoint_io(
            state,
            deadline,
            store.commit_dispatch(&checkpoint, pipeline.handlers().store().take_batch()),
        )
        .await?;
        committed = Some(checkpoint);
        if connected {
            *state.write().await = GatewayState::Connected;
        }
    }
    warn!("gateway shard stream ended; supervisor reports down until restart");
    Ok(())
}

/// Build the supervisor's shard: single-shard deployment (one guild, ADR
/// 0001) over [`intents_from_env`].
///
/// A stored [`Session`] (S5) resumes the previous gateway session instead of
/// a fresh IDENTIFY.
#[must_use]
pub fn build_shard(token: String, intents: Intents, session: Option<&GatewaySession>) -> Shard {
    Shard::with_config(ShardId::ONE, build_shard_config(token, intents, session))
}

pub fn build_shard_config(
    token: String,
    intents: Intents,
    session: Option<&GatewaySession>,
) -> twilight_gateway::Config {
    use twilight_gateway::ConfigBuilder;
    let mut builder = ConfigBuilder::new(token, intents);
    // Both fields are required to resume the saved session at its proper URL.
    // Source: https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.ConfigBuilder.html#method.session
    if let Some(session) = session {
        builder = builder
            .session(Session::new(session.sequence, session.session_id.clone()))
            .resume_url(session.resume_url.clone());
    }
    builder.build()
}

#[must_use]
pub fn build_pipeline(milestones: Vec<FunnelEvent>) -> GatewayPipeline {
    Pipeline::new(
        GatewayFunnelBuffer::from_milestones(milestones),
        None,
        None,
        NoInvites,
        NoClassification,
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
    async fn checkpoint_io_deadline_includes_pending_operation_and_leaves_unready() {
        let state = RwLock::new(GatewayState::Connected);
        let result = checkpoint_io(
            &state,
            std::time::Duration::from_millis(10),
            std::future::pending::<Result<(), sqlx::Error>>(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(*state.read().await, GatewayState::Armed);
    }

    #[tokio::test]
    async fn checkpoint_io_restores_readiness_only_after_success() {
        let state = RwLock::new(GatewayState::Connected);
        checkpoint_io(&state, CHECKPOINT_IO_MAX, async {
            assert_eq!(*state.read().await, GatewayState::Armed);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(*state.read().await, GatewayState::Connected);
        let result = checkpoint_io(&state, CHECKPOINT_IO_MAX, async {
            Err::<(), _>(sqlx::Error::InvalidArgument("test failure".into()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*state.read().await, GatewayState::Armed);
    }

    #[tokio::test]
    async fn fresh_shard_has_no_session_to_persist() {
        ensure_crypto_provider();
        let shard = build_shard("token".to_owned(), Intents::empty(), None);
        assert_eq!(session_snapshot(&shard), None);
    }
}
