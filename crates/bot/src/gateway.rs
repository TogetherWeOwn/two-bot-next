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

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use futures_util::StreamExt as _;
use tokio::sync::RwLock;
use tracing::{info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Message, Session, Shard, ShardId};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{
    boot_action, dispatch_action, invalidates_session, BootAction, DispatchAction, GatewaySession,
};
use two_bot_core::{ComponentStatus, Config, InviteState, NoopFacts, NoopLeveling, Snowflake};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{
    gateway_intents, needs_message_content, InviteSource, NoClassification, Pipeline,
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
    /// No token configured; the shard will not start.
    Unconfigured,
    /// Token present; the supervisor task is (re)connecting.
    Armed,
    /// Shard connected and identified (constructed by the supervisor,
    /// exercised by the /readyz test).
    Connected,
    /// Reception has stopped; effects are draining before process restart.
    Draining,
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
            Self::Draining => ComponentStatus::Down,
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

pub type GatewayPipeline<I = two_bot_discord::NoInvites> = Pipeline<
    GatewayFunnelBuffer,
    NoopLeveling,
    NoopFacts,
    I,
    NoClassification,
    GatewayFunnelBuffer,
>;

pub async fn load_boot_session(
    store: &GatewaySessionStore,
) -> Result<Option<GatewaySession>, sqlx::Error> {
    tokio::time::timeout(CHECKPOINT_IO_MAX, async {
        let saved = store.load().await?;
        match boot_action(saved.as_ref(), two_bot_core::funnel::now_millis_for_test()) {
            BootAction::Resume => Ok(saved),
            BootAction::DiscardAndIdentify => {
                store.clear().await?;
                Ok(None)
            }
            BootAction::Identify => Ok(None),
        }
    })
    .await
    .map_err(|_| sqlx::Error::InvalidArgument("gateway boot deadline exceeded".into()))?
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

/// Client-side bound from pool acquisition through COMMIT, including stalled
/// responses on an acquired connection. Never restore readiness during drain.
async fn checkpoint_io<T>(
    state: &RwLock<GatewayState>,
    generation: &AtomicU64,
    deadline: std::time::Duration,
    operation: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    let (previous, observed_generation) = {
        let mut state = state.write().await;
        let previous = *state;
        if previous != GatewayState::Draining {
            *state = GatewayState::Armed;
        }
        (previous, generation.load(Ordering::Acquire))
    };
    let result = tokio::time::timeout(deadline, operation)
        .await
        .map_err(|_| sqlx::Error::InvalidArgument("gateway checkpoint deadline exceeded".into()))?;
    if result.is_ok() {
        let mut state = state.write().await;
        if *state != GatewayState::Draining
            && generation.load(Ordering::Acquire) == observed_generation
        {
            *state = previous;
        }
    }
    result
}

async fn transport_disconnected(state: &RwLock<GatewayState>, generation: &AtomicU64) {
    // Share the lock with checkpoint restoration and READY publication so a
    // disconnect cannot land between their generation check and state write.
    let mut state = state.write().await;
    generation.fetch_add(1, Ordering::AcqRel);
    if *state != GatewayState::Draining {
        *state = GatewayState::Armed;
    }
}

struct ReceivedDispatch {
    event: Event,
    observed_at: String,
}
#[cfg(test)]
impl ReceivedDispatch {
    fn new(event: Event) -> Self {
        Self {
            event,
            observed_at: two_bot_core::now_iso(),
        }
    }
}

enum ReceivedWork {
    Clear(std::time::Duration),
    Dispatch {
        dispatch: Option<Box<ReceivedDispatch>>,
        checkpoint: GatewaySession,
        deadline: std::time::Duration,
        generation: u64,
    },
    Failed,
}

/// Raw packets retain unmapped dispatch sequences too. Poll transport separately
/// from the serial effects/checkpoint writer. Metadata may advance in reception;
/// only the worker's successful transaction advances the durable replay cursor.
pub async fn run_shard<I: InviteSource + 'static>(
    shard: Shard,
    pipeline: Arc<GatewayPipeline<I>>,
    state: Arc<RwLock<GatewayState>>,
    store: GatewaySessionStore,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), sqlx::Error> {
    let generation = Arc::new(AtomicU64::new(0));
    let saved = checkpoint_io(&state, &generation, CHECKPOINT_IO_MAX, store.load()).await?;
    let receive_generation = Arc::clone(&generation);
    let receive_state = Arc::clone(&state);
    let events = futures_util::stream::unfold(
        (shard, saved, CHECKPOINT_IO_MAX),
        move |(mut shard, mut received, mut deadline)| {
            let state = Arc::clone(&receive_state);
            let generation = Arc::clone(&receive_generation);
            async move {
                // Failure is emitted to the ordered worker, never skipped past.
                let work: Result<Option<ReceivedWork>, sqlx::Error> = async {
                    while let Some(item) = shard.next().await {
                        let message = match item {
                            Ok(message) => message,
                            Err(error) if matches!(error.kind(), twilight_gateway::error::ReceiveMessageErrorType::Reconnect) => {
                                transport_disconnected(&state, &generation).await;
                                warn!("gateway reconnect failed; Twilight will retry");
                                continue;
                            }
                            Err(_) => return Err(sqlx::Error::InvalidArgument("gateway receive failed".into())),
                        };
                        let Message::Text(text) = message else {
                            transport_disconnected(&state, &generation).await;
                            let rejected = matches!(message, Message::Close(Some(ref frame)) if matches!(frame.code, 4007 | 4009));
                            let clear = rejected || shard.session().is_none();
                            if rejected { shard = Shard::with_config(shard.id(), shard.config().clone()); }
                            if clear { received = None; return Ok(Some(ReceivedWork::Clear(deadline))); }
                            continue;
                        };
                        let observed_at = two_bot_core::now_iso();
                        let header: Header = serde_json::from_str(&text).map_err(|_| sqlx::Error::InvalidArgument("invalid gateway header".into()))?;
                        if header.op == 10 {
                            let hello: HelloPacket = serde_json::from_str(&text).map_err(|_| sqlx::Error::InvalidArgument("invalid gateway hello".into()))?;
                            if hello.d.heartbeat_interval == 0 { return Err(sqlx::Error::InvalidArgument("zero heartbeat interval".into())); }
                            deadline = CHECKPOINT_IO_MAX.min(std::time::Duration::from_millis(hello.d.heartbeat_interval) / 4);
                        }
                        if header.op == 9 {
                            let packet: serde_json::Value = serde_json::from_str(&text).map_err(|_| sqlx::Error::InvalidArgument("invalid gateway session packet".into()))?;
                            let resumable = packet["d"].as_bool().ok_or_else(|| sqlx::Error::InvalidArgument("invalid gateway session flag".into()))?;
                            transport_disconnected(&state, &generation).await;
                            if invalidates_session(resumable) { received = None; return Ok(Some(ReceivedWork::Clear(deadline))); }
                        }
                        if header.op != 0 { continue; }
                        let sequence = header.s.ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing sequence".into()))?;
                        let session = session_snapshot(&shard).ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing session".into()))?;
                        let resume_url = shard.resume_url().or_else(|| received.as_ref().filter(|saved| saved.session_id == session.id()).map(|saved| saved.resume_url.as_str()))
                            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing resume URL".into()))?;
                        let checkpoint = GatewaySession { session_id: session.id().to_owned(), sequence, resume_url: resume_url.to_owned(), updated_at_ms: two_bot_core::funnel::now_millis_for_test() };
                        if dispatch_action(received.as_ref(), &checkpoint.session_id, sequence) == DispatchAction::Duplicate { continue; }
                        let parsed = twilight_gateway::parse(text, EventTypeFlags::all()).map_err(|_| sqlx::Error::InvalidArgument("gateway dispatch parse failed".into()))?;
                        received = Some(checkpoint.clone());
                        let dispatch = parsed.map(|parsed| Box::new(ReceivedDispatch { event: Event::from(parsed), observed_at }));
                        return Ok(Some(ReceivedWork::Dispatch { dispatch, checkpoint, deadline, generation: generation.load(std::sync::atomic::Ordering::Acquire) }));
                    }
                    Ok(None)
                }.await;
                match work {
                    Ok(Some(work)) => Some((work, (shard, received, deadline))),
                    Ok(None) => None,
                    Err(_) => Some((ReceivedWork::Failed, (shard, received, deadline))),
                }
            }
        },
    );
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    let handle = tokio::runtime::Handle::current();
    let worker_state = Arc::clone(&state);
    let stop_state = Arc::clone(&state);
    let result = crate::dispatch::dispatch_bounded(
        // Ending reception is cooperative: dispatch_bounded keeps supervising
        // and draining its blocking writer instead of being aborted/dropped.
        events.take_until(shutdown),
        crate::dispatch::DISPATCH_BACKLOG,
        move |work| match work {
            ReceivedWork::Clear(deadline) => handle
                .block_on(checkpoint_io(
                    &worker_state,
                    &generation,
                    deadline,
                    store.clear(),
                ))
                .unwrap_or_else(|_| panic!("gateway clear failed")),
            ReceivedWork::Failed => panic!("gateway receive failed; checkpoint unchanged"),
            ReceivedWork::Dispatch {
                dispatch,
                checkpoint,
                deadline,
                generation: observed_generation,
            } => {
                let mut connected = false;
                if let Some(dispatch) = dispatch {
                    connected = matches!(dispatch.event, Event::Ready(_) | Event::Resumed);
                    pipeline.handle_at(&dispatch.event, &dispatch.observed_at);
                }
                handle
                    .block_on(checkpoint_io(
                        &worker_state,
                        &generation,
                        deadline,
                        store
                            .commit_dispatch(&checkpoint, pipeline.handlers().store().take_batch()),
                    ))
                    .unwrap_or_else(|_| panic!("gateway checkpoint failed"));
                if connected {
                    let mut state = handle.block_on(worker_state.write());
                    if *state != GatewayState::Draining
                        && generation.load(Ordering::Acquire) == observed_generation
                    {
                        *state = GatewayState::Connected;
                    }
                }
            }
        },
        move || async move {
            *stop_state.write().await = GatewayState::Draining;
        },
        crate::dispatch::DISPATCH_IO_MAX,
        crate::dispatch::DISPATCH_DRAIN_MAX,
    )
    .await;
    // Reception does not restart in this runner. Keep Draining sticky through
    // both successful shutdown and fatal exit, including any remaining writer.
    result.map_err(|reason| sqlx::Error::InvalidArgument(reason.into()))
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

#[cfg(test)]
#[must_use]
pub fn build_pipeline(milestones: Vec<two_bot_core::FunnelEvent>) -> GatewayPipeline {
    let buffer = GatewayFunnelBuffer::from_milestones(milestones);
    Pipeline::with_snapshots(
        buffer.clone(),
        Some(NoopLeveling),
        Some(NoopFacts),
        two_bot_discord::NoInvites,
        NoClassification,
        buffer,
    )
}

pub async fn build_persistent_pipeline(
    store: &GatewaySessionStore,
    guild_id: Snowflake,
    token: String,
) -> Result<GatewayPipeline<HttpInvites>, sqlx::Error> {
    tokio::time::timeout(CHECKPOINT_IO_MAX, async {
        let buffer = GatewayFunnelBuffer::from_milestones(store.milestones().await?);
        buffer.seed_snapshots(guild_id, store.invite_snapshots().await?);
        Ok(Pipeline::with_snapshots(
            buffer.clone(),
            Some(NoopLeveling),
            Some(NoopFacts),
            HttpInvites {
                client: twilight_http::Client::new(token),
                handle: tokio::runtime::Handle::current(),
            },
            NoClassification,
            buffer,
        ))
    })
    .await
    .map_err(|_| sqlx::Error::InvalidArgument("gateway baseline deadline exceeded".into()))?
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
            &AtomicU64::new(0),
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
        checkpoint_io(&state, &AtomicU64::new(0), CHECKPOINT_IO_MAX, async {
            assert_eq!(*state.read().await, GatewayState::Armed);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(*state.read().await, GatewayState::Connected);
        let result = checkpoint_io(&state, &AtomicU64::new(0), CHECKPOINT_IO_MAX, async {
            Err::<(), _>(sqlx::Error::InvalidArgument("test failure".into()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*state.read().await, GatewayState::Armed);
    }

    #[tokio::test]
    async fn checkpoint_completion_cannot_overwrite_a_transport_disconnect() {
        let state = RwLock::new(GatewayState::Connected);
        let generation = AtomicU64::new(0);
        checkpoint_io(&state, &generation, CHECKPOINT_IO_MAX, async {
            assert_eq!(*state.read().await, GatewayState::Armed);
            transport_disconnected(&state, &generation).await;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(generation.load(Ordering::Acquire), 1);
        assert_eq!(*state.read().await, GatewayState::Armed);
        assert_ne!(state.read().await.status(), ComponentStatus::Ready);
    }

    #[tokio::test]
    async fn transport_disconnect_cannot_clear_draining() {
        let state = RwLock::new(GatewayState::Draining);
        transport_disconnected(&state, &AtomicU64::new(0)).await;
        assert_eq!(*state.read().await, GatewayState::Draining);
    }

    #[tokio::test]
    async fn checkpoint_completion_cannot_restore_readiness_after_reception_stops() {
        let state = RwLock::new(GatewayState::Connected);
        checkpoint_io(&state, &AtomicU64::new(0), CHECKPOINT_IO_MAX, async {
            *state.write().await = GatewayState::Draining;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(*state.read().await, GatewayState::Draining);
        assert_eq!(state.read().await.status(), ComponentStatus::Down);
    }

    #[tokio::test]
    async fn checkpoint_timeout_cancels_never_completing_client_operation() {
        struct Cancelled(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Cancelled(Arc::clone(&cancelled));
        let state = RwLock::new(GatewayState::Connected);
        let result = checkpoint_io(
            &state,
            &AtomicU64::new(0),
            std::time::Duration::from_millis(10),
            async move {
                let _guard = guard;
                std::future::pending::<Result<(), sqlx::Error>>().await
            },
        )
        .await;
        assert!(result.is_err());
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(*state.read().await, GatewayState::Armed);
    }

    #[tokio::test]
    async fn fresh_shard_has_no_session_to_persist() {
        ensure_crypto_provider();
        let shard = build_shard("token".to_owned(), Intents::empty(), None);
        assert_eq!(session_snapshot(&shard), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_voice_gate_and_leave_use_receipt_time() {
        use std::sync::Mutex;
        use std::time::Duration;
        use twilight_model::gateway::payload::incoming::{
            MemberRemove, MemberUpdate, VoiceStateUpdate,
        };
        use two_bot_core::EventType;
        use two_bot_discord::MemPipeline;

        let user: twilight_model::user::User = serde_json::from_value(serde_json::json!({
            "id": "123", "username": "fixture", "discriminator": "0001", "avatar": null
        }))
        .unwrap();
        let gate = Event::MemberUpdate(Box::new(
            serde_json::from_value::<MemberUpdate>(serde_json::json!({
                "guild_id": "456", "user": user, "roles": [], "pending": false
            }))
            .unwrap(),
        ));
        let voice = |channel: Option<&str>| {
            Event::VoiceStateUpdate(Box::new(
                serde_json::from_value::<VoiceStateUpdate>(serde_json::json!({
                    "guild_id": "456", "user_id": "123", "channel_id": channel,
                    "session_id": "fixture", "deaf": false, "mute": false,
                    "self_deaf": false, "self_mute": false, "self_video": false,
                    "suppress": false
                }))
                .unwrap(),
            ))
        };
        let leave = Event::MemberRemove(MemberRemove {
            guild_id: twilight_model::id::Id::new(456),
            user,
        });
        let events = vec![Event::Resumed, gate, voice(Some("789")), voice(None), leave];
        let pipeline = Arc::new(MemPipeline::for_replay());
        let worker_pipeline = Arc::clone(&pipeline);
        let (release, wait) = std::sync::mpsc::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let stamps = Arc::clone(&observed);
        let stream = futures_util::stream::unfold(
            (0, events.into_iter(), Some(start)),
            move |(n, mut events, mut start)| {
                let release = release.clone();
                let stamps = Arc::clone(&stamps);
                async move {
                    if n == 1 {
                        start.take().unwrap().await.unwrap();
                    }
                    if n == 3 {
                        tokio::time::sleep(Duration::from_millis(1300)).await;
                    }
                    let Some(event) = events.next() else {
                        release.send(()).unwrap();
                        return None;
                    };
                    let dispatch = ReceivedDispatch::new(event);
                    stamps.lock().unwrap().push(dispatch.observed_at.clone());
                    Some((dispatch, (n + 1, events, start)))
                }
            },
        );
        let mut started = Some(started);
        let result = crate::dispatch::dispatch_ordered(stream, 8, move |dispatch| {
            if matches!(dispatch.event, Event::Resumed) {
                started.take().unwrap().send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            worker_pipeline.handle_at(&dispatch.event, &dispatch.observed_at);
        })
        .await;
        assert_eq!(result, Ok(()));
        let stamps = observed.lock().unwrap();
        let rows = pipeline.handlers().store().rows();
        for (kind, index) in [
            (EventType::GateCleared, 1),
            (EventType::VoiceSessionStart, 2),
            (EventType::VoiceSessionEnd, 3),
            (EventType::MemberLeave, 4),
        ] {
            let row = rows.iter().find(|row| row.event_type == kind).unwrap();
            assert_eq!(row.occurred_at, stamps[index], "{kind:?} lost receipt time");
        }
        let end = rows
            .iter()
            .find(|row| row.event_type == EventType::VoiceSessionEnd)
            .unwrap();
        assert!(
            end.metadata.as_ref().unwrap()["durationSeconds"]
                .as_f64()
                .unwrap()
                >= 1.0
        );
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
