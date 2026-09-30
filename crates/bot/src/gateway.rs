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

struct BufferedPacket {
    message: Result<Message, twilight_gateway::error::ReceiveMessageError>,
    session: Option<Session>,
    resume_url: Option<String>,
    acknowledgement: Option<
        tokio::task::JoinHandle<
            Result<two_bot_discord::rsvp::PreparedRsvp, two_bot_discord::DiscordError>,
        >,
    >,
}

impl BufferedPacket {
    fn capture(
        shard: &Shard,
        message: Result<Message, twilight_gateway::error::ReceiveMessageError>,
        runtime: Option<&Arc<two_bot_discord::interactions::InteractionRuntime>>,
        committed: Option<&GatewaySession>,
    ) -> Self {
        let mut packet = Self {
            message,
            session: session_snapshot(shard),
            resume_url: shard.resume_url().map(str::to_owned),
            acknowledgement: None,
        };
        packet.prepare(runtime, committed);
        packet
    }

    fn prepare(
        &mut self,
        runtime: Option<&Arc<two_bot_discord::interactions::InteractionRuntime>>,
        committed: Option<&GatewaySession>,
    ) {
        let (Some(runtime), Ok(Message::Text(text)), Some(session)) =
            (runtime, &self.message, &self.session)
        else {
            return;
        };
        let Ok(header) = serde_json::from_str::<Header>(text) else {
            return;
        };
        let Some(sequence) = header.s.filter(|_| header.op == 0) else {
            return;
        };
        if dispatch_action(committed, session.id(), sequence) == DispatchAction::Duplicate {
            return;
        }
        if let Ok(Some(parsed)) =
            twilight_gateway::parse(text.clone(), EventTypeFlags::INTERACTION_CREATE)
        {
            if let Event::InteractionCreate(interaction) = Event::from(parsed) {
                let runtime = Arc::clone(runtime);
                self.acknowledgement =
                    Some(tokio::spawn(
                        async move { runtime.prepare(interaction.0).await },
                    ));
            }
        }
    }
}

const MAX_PENDING_PACKETS: usize = 64;
const FEATURE_IO_MAX: std::time::Duration = std::time::Duration::from_secs(30);

impl BufferedPacket {
    fn is_boundary(&self) -> bool {
        match &self.message {
            Ok(Message::Text(text)) => serde_json::from_str::<Header>(text)
                .map_or(true, |header| matches!(header.op, 7 | 9)),
            Ok(Message::Close(_)) | Err(_) => true,
        }
    }
}

/// Poll Twilight while awaiting feature I/O, but retain dispatch order and do
/// not advance the durable checkpoint ahead of command effects. Capture session
/// identity at receive time: a later reconnect must not relabel buffered packets.
/// Stop read-ahead at a reconnect boundary until the loop handles it, otherwise
/// rebuilding a rejected shard could discard an already-buffered new session.
async fn poll_while<S, P>(
    stream: &mut S,
    pending: &mut std::collections::VecDeque<P>,
    capture: impl Fn(&S, S::Item) -> P,
    is_boundary: impl Fn(&P) -> bool,
    operation: impl std::future::Future<Output = Result<(), two_bot_discord::DiscordError>>,
) -> Result<(), sqlx::Error>
where
    S: futures_util::Stream + Unpin,
{
    tokio::pin!(operation);
    let timeout = tokio::time::sleep(FEATURE_IO_MAX);
    tokio::pin!(timeout);
    let mut paused = pending.iter().any(&is_boundary);
    let mut slow = false;
    loop {
        tokio::select! {
            biased;
            result = &mut operation => return result.map_err(|_| sqlx::Error::InvalidArgument("interaction execution failed".into())),
            _ = &mut timeout, if !slow => {
                // Cancellation after defer loses accepted work: a replay cannot
                // acknowledge it again. Drain to a final reply and checkpoint.
                slow = true;
                warn!("gateway feature I/O slow; draining accepted command");
            }
            item = stream.next(), if !paused && pending.len() < MAX_PENDING_PACKETS => {
                let Some(item) = item else {
                    paused = true;
                    continue;
                };
                let packet = capture(stream, item);
                paused = is_boundary(&packet);
                pending.push_back(packet);
            }
        }
    }
}

/// Drive raw packets so even dispatches not mapped by Twilight have a durable
/// sequence. Twilight itself still owns transport, heartbeat and opcode-9
/// fallback. Source: https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Shard.html
pub async fn run_shard(
    mut shard: Shard,
    pipeline: Arc<GatewayPipeline>,
    state: Arc<RwLock<GatewayState>>,
    store: GatewaySessionStore,
    interactions: Option<Arc<two_bot_discord::interactions::InteractionRuntime>>,
) -> Result<(), sqlx::Error> {
    let result = run_loop(&mut shard, &pipeline, &state, &store, interactions.as_ref()).await;
    *state.write().await = GatewayState::Armed;
    result
}

async fn run_loop(
    shard: &mut Shard,
    pipeline: &GatewayPipeline,
    state: &RwLock<GatewayState>,
    store: &GatewaySessionStore,
    interactions: Option<&Arc<two_bot_discord::interactions::InteractionRuntime>>,
) -> Result<(), sqlx::Error> {
    if let Some(runtime) = interactions {
        runtime.publish_current().await.map_err(|_| {
            sqlx::Error::InvalidArgument("interaction registry boot sync failed".into())
        })?;
    }
    let mut deadline = CHECKPOINT_IO_MAX;
    let mut committed = checkpoint_io(state, deadline, store.load()).await?;
    let mut pending = std::collections::VecDeque::new();
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    loop {
        let packet = match pending.pop_front() {
            Some(packet) => packet,
            None => match shard.next().await {
                Some(item) => {
                    BufferedPacket::capture(shard, item, interactions, committed.as_ref())
                }
                None => break,
            },
        };
        let message = match packet.message {
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
            if rejected || packet.session.is_none() {
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
        let session = packet
            .session
            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing session".into()))?;
        // Twilight drops resume_url on a failed connect, but retains the session
        // and may successfully RESUME at its bootstrap endpoint. RESUMED carries
        // no new URL: retain READY's committed URL only for this same session.
        let resume_url = packet
            .resume_url
            .as_deref()
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
            if let Some(runtime) = interactions {
                if let Some(acknowledgement) = packet.acknowledgement {
                    let operation = async {
                        let result = match acknowledgement.await {
                            Ok(Ok(prepared)) => runtime.complete(prepared).await,
                            Ok(Err(error)) => Err(error),
                            Err(_) => Err(two_bot_discord::DiscordError::Rejected(
                                "interaction acknowledgement task failed".into(),
                            )),
                        };
                        if result.is_err() {
                            // Do not retry committed effects after an uncertain
                            // reply, or log errors containing interaction tokens.
                            warn!("interaction response failed; not replaying command");
                        }
                        Ok(())
                    };
                    poll_while(
                        shard,
                        &mut pending,
                        |shard, item| {
                            BufferedPacket::capture(shard, item, interactions, committed.as_ref())
                        },
                        BufferedPacket::is_boundary,
                        operation,
                    )
                    .await?;
                }
            }
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

    #[tokio::test]
    async fn feature_io_keeps_polling_and_preserves_packet_order() {
        let mut stream = futures_util::stream::iter(0..10).chain(futures_util::stream::pending());
        let mut pending = std::collections::VecDeque::new();
        let polled = std::cell::Cell::new(0);
        poll_while(
            &mut stream,
            &mut pending,
            |_, packet| {
                polled.set(polled.get() + 1);
                packet
            },
            |_| false,
            async {
                // A sleeping feature must not prevent transport/heartbeat polling.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                assert_eq!(polled.get(), 10);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn feature_read_ahead_stops_at_reconnect_boundary() {
        let mut stream = futures_util::stream::iter([1, 9, 2]);
        let mut pending = std::collections::VecDeque::new();
        for _ in 0..2 {
            poll_while(
                &mut stream,
                &mut pending,
                |_, packet| packet,
                |packet| *packet == 9,
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(pending.iter().copied().collect::<Vec<_>>(), vec![1, 9]);
        }
        // The loop must process the boundary before Twilight reads a new session.
        assert_eq!(stream.next().await, Some(2));
    }

    #[tokio::test]
    async fn feature_backlog_is_bounded_without_cancelling_accepted_work() {
        let mut stream = futures_util::stream::iter(0..MAX_PENDING_PACKETS + 1);
        let mut pending = std::collections::VecDeque::new();
        let completed = std::cell::Cell::new(false);
        poll_while(
            &mut stream,
            &mut pending,
            |_, packet| packet,
            |_| false,
            async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                completed.set(true);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(completed.get());
        assert_eq!(pending.len(), MAX_PENDING_PACKETS);
        assert_eq!(stream.next().await, Some(MAX_PENDING_PACKETS));
    }

    #[tokio::test]
    async fn feature_deadline_and_stream_end_do_not_cancel_accepted_work() {
        let mut stream = futures_util::stream::empty::<usize>();
        let mut pending = std::collections::VecDeque::new();
        let completed = std::cell::Cell::new(false);
        poll_while(
            &mut stream,
            &mut pending,
            |_, packet| packet,
            |_| false,
            async {
                tokio::time::sleep(FEATURE_IO_MAX + std::time::Duration::from_millis(10)).await;
                completed.set(true);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(completed.get());
    }

    #[test]
    fn reconnect_packets_are_read_ahead_boundaries() {
        for text in [r#"{"op":7}"#, r#"{"op":9,"d":false}"#, "invalid"] {
            assert!(BufferedPacket {
                message: Ok(Message::Text(text.into())),
                session: None,
                resume_url: None,
                acknowledgement: None,
            }
            .is_boundary());
        }
        assert!(BufferedPacket {
            message: Ok(Message::Close(None)),
            session: None,
            resume_url: None,
            acknowledgement: None,
        }
        .is_boundary());
        assert!(!BufferedPacket {
            message: Ok(Message::Text(r#"{"op":0,"s":2,"t":"RESUMED"}"#.into())),
            session: None,
            resume_url: None,
            acknowledgement: None,
        }
        .is_boundary());
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
