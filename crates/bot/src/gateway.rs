//! Gateway shard supervisor (S3).
//!
//! Owns the shard lifecycle state the /readyz gate reads and runs the
//! twilight [`Shard`] event loop. Every gateway dispatch goes through the
//! [`GatewayPipeline`]: the cache updates inside `handle()`, so dispatch here is
//! one line plus the `Error` row (parity matrix §3: legacy `client_error`
//! log → `tracing::warn!`).
//!
//! RESUME across Container restarts uses the last committed dispatch sequence.
//! The pipeline drops open voice sessions on both READY and RESUMED. Every
//! funnel batch commits with its checkpoint; persistence/dispatch failures
//! stop the runner rather than checkpointing ahead of uncommitted effects.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::onboarding::OnboardingJob;

use futures_util::StreamExt as _;
use tokio::sync::RwLock;
use tracing::{info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Message, Session, Shard, ShardId};
use two_bot::voice_rooms::{build_production_runtime, VoiceEventSink};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{
    boot_action_with, dispatch_action, invalidates_session, BootAction, DispatchAction,
    GatewaySession,
};
use two_bot_core::{ComponentStatus, Config, InviteState, Snowflake};
use two_bot_cutover::gateway_session::{GatewayJob, GatewaySessionStore};
use two_bot_cutover::{connect, DB_POOL_MAX_DEFAULT};
use two_bot_discord::{
    gateway_intents, needs_message_content, InviteSource, LevelingRuntime, OrderedLevelingPipeline,
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

/// Gateway pipeline: ordered leveling awards over the persistent funnel
/// buffer. `I` serves invite counters (HTTP at runtime, none in tests); the
/// funnel buffer doubles as the invite snapshot store, seeded at boot.
pub type GatewayPipeline<I = two_bot_discord::NoInvites> =
    OrderedLevelingPipeline<GatewayFunnelBuffer, I, GatewayFunnelBuffer>;

pub async fn load_boot_session(
    store: &GatewaySessionStore,
) -> Result<Option<GatewaySession>, sqlx::Error> {
    tokio::time::timeout(CHECKPOINT_IO_MAX, async {
        let (saved, directive) = store.load_for_boot().await?;
        match boot_action_with(
            saved.as_ref(),
            directive,
            two_bot_core::funnel::now_millis_for_test(),
        ) {
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

// The ordered checkpoint writer gets its own pool, distinct from the shared
// store pool feature work runs on: feature transactions retain connections
// across Discord I/O. Saturation tests model a tighter feature budget.
pub const GATEWAY_POOL_MAX: u32 = 1;
#[cfg(test)]
pub const FEATURE_POOL_MAX: u32 = two_bot_cutover::DB_POOL_MAX_DEFAULT - GATEWAY_POOL_MAX;
// Admission is independent of the 32-row durable queue. Leave feature capacity
// for settings and shared commands; do not claim more jobs while these workers run.
const ONBOARDING_WORKER_LIMIT: usize = 2;
// Bound detached initial ACKs started at reception.
const ACK_LIMIT: usize = 32;

const CHECKPOINT_IO_MAX: std::time::Duration = std::time::Duration::from_secs(5);

struct LiveInteraction {
    interaction: Box<twilight_model::application::interaction::Interaction>,
    ticket: tokio::sync::oneshot::Receiver<bool>,
    generation: u64,
}

type LiveInteractions = Arc<std::sync::Mutex<HashMap<i64, LiveInteraction>>>;

async fn generation_changed(generation: &AtomicU64, expected: u64) {
    while generation.load(Ordering::Acquire) == expected {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

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
/// Source: <https://docs.rs/tokio/1/tokio/time/fn.timeout.html>
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

type RsvpAcknowledgement = tokio::task::JoinHandle<
    Result<two_bot_discord::rsvp::PreparedRsvp, two_bot_discord::DiscordError>,
>;

/// Completes one acknowledged command; returns whether the checkpoint may
/// advance past it. Only an admission-Blocked preparation exhaustion returns
/// false: the callback never reached Discord, so the cursor must hold instead
/// of silently passing a never-acknowledged command. Every other preparation
/// failure, task failure and completion failure keeps the existing warn-and-
/// advance behavior.
async fn complete_acknowledgement(
    runtime: &two_bot_discord::interactions::InteractionRuntime,
    acknowledgement: RsvpAcknowledgement,
) -> bool {
    let prepared = match acknowledgement.await {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            if error.is_admission_blocked() {
                warn!("interaction acknowledgement blocked; checkpoint unchanged");
                return false;
            }
            // Do not replay uncertain effects or log interaction tokens.
            warn!("interaction response failed; not replaying command");
            return true;
        }
        Err(_) => {
            warn!("interaction response failed; not replaying command");
            return true;
        }
    };
    if runtime.complete(prepared).await.is_err() {
        // Do not replay uncertain effects or log interaction tokens.
        warn!("interaction response failed; not replaying command");
    }
    true
}

struct AcceptedRsvp {
    acknowledgement: RsvpAcknowledgement,
    completed: tokio::sync::oneshot::Sender<bool>,
}

/// Reception acknowledges immediately; completion stays serial and supervised
/// even if the funnel writer fails. The dispatch backlog bounds admission.
fn start_rsvp_drain(
    runtime: Arc<two_bot_discord::interactions::InteractionRuntime>,
) -> (
    tokio::sync::mpsc::UnboundedSender<AcceptedRsvp>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AcceptedRsvp>();
    let task = tokio::spawn(async move {
        while let Some(accepted) = receiver.recv().await {
            let acknowledged = complete_acknowledgement(&runtime, accepted.acknowledgement).await;
            let _ = accepted.completed.send(acknowledged);
        }
    });
    (sender, task)
}

struct ReceivedDispatch {
    event: Event,
    observed_at: String,
    completion: Option<tokio::sync::oneshot::Receiver<bool>>,
}
#[cfg(test)]
impl ReceivedDispatch {
    fn new(event: Event) -> Self {
        Self {
            event,
            observed_at: two_bot_core::now_iso(),
            completion: None,
        }
    }
}

enum ReceivedWork {
    Clear(std::time::Duration),
    Dispatch {
        dispatch: Option<Box<ReceivedDispatch>>,
        /// A MESSAGE_UPDATE decoded from its raw dispatch before Twilight's
        /// parse, with its receipt stamp. Only set while automod is active.
        edit: Option<Box<(two_bot_core::automod_runtime::MessageDelivery, String)>>,
        checkpoint: GatewaySession,
        deadline: std::time::Duration,
        generation: u64,
        acknowledgement: Option<tokio::sync::oneshot::Receiver<bool>>,
        /// Cold voice RESUME: reception waits for this commit before IDENTIFY.
        committed: Option<tokio::sync::oneshot::Sender<()>>,
    },
    Failed(sqlx::Error),
}

/// Covers stream termination, receive/checkpoint failures and task
/// cancellation, not just close frames observed at reception.
struct VoiceConnectionGuard(Option<Arc<dyn VoiceEventSink>>);

impl Drop for VoiceConnectionGuard {
    fn drop(&mut self) {
        if let Some(voice) = &self.0 {
            voice.disconnect();
        }
    }
}

fn voice_disconnected(voice: Option<&Arc<dyn VoiceEventSink>>) {
    if let Some(voice) = voice {
        voice.disconnect();
    }
}

/// Raw packets retain unmapped dispatch sequences too. Poll transport separately
/// from the serial effects/checkpoint writer. Metadata may advance in reception;
/// only the worker's successful transaction advances the durable replay cursor.
/// Twilight owns transport, heartbeat and opcode-9 fallback.
/// Source: <https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Shard.html>
///
/// The shared command runtime dispatches detached work at reception (after the
/// replay guard), so slash acknowledgements do not queue behind slow funnel I/O.
/// It never awaits REST/store work on the transport polling path. Ordered
/// XP/reward processing runs on the serial checkpoint writer: the funnel half
/// stays synchronous under the checkpoint deadline, then deferred awards drain
/// through the leveling runtime before the cursor commits. Shutdown ends
/// reception cooperatively so the bounded writer remains supervised through drain.
///
/// `voice` is the V1 voice sink (TOG-10093), `None` unless `TWO_VOICE=1` with
/// token + database configured (see [`build_voice_runtime`]). The serial
/// writer feeds it after each cache update; reception invalidates occupancy
/// at every transport loss, so a disconnect is never deferred behind backlog.
#[allow(clippy::too_many_arguments)]
pub async fn run_shard<I: InviteSource + 'static>(
    shard: Shard,
    pipeline: Arc<GatewayPipeline<I>>,
    state: Arc<RwLock<GatewayState>>,
    store: GatewaySessionStore,
    interactions: Option<Arc<two_bot_discord::interactions::InteractionRuntime>>,
    onboarding: Option<Arc<crate::onboarding::OnboardingRuntime>>,
    runtime: Option<Arc<crate::command_runtime::CommandRuntime>>,
    automod: Option<Arc<crate::automod_gateway::ProductionAutomod>>,
    voice: Option<Arc<dyn VoiceEventSink>>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), sqlx::Error> {
    let _dispatch_guard = runtime.as_ref().map(|runtime| runtime.dispatch_guard());
    let _voice_connection = VoiceConnectionGuard(voice.clone());
    let generation = Arc::new(AtomicU64::new(0));
    let saved = checkpoint_io(&state, &generation, CHECKPOINT_IO_MAX, store.load()).await?;
    if let Some(runtime) = interactions.as_ref() {
        runtime.publish_current().await.map_err(|_| {
            sqlx::Error::InvalidArgument("interaction registry boot sync failed".into())
        })?;
    }
    let (rsvp_sender, rsvp_drain) = match interactions.as_ref() {
        Some(runtime) => {
            let (sender, task) = start_rsvp_drain(Arc::clone(runtime));
            (Some(sender), Some(task))
        }
        None => (None, None),
    };
    let receive_rsvp = rsvp_sender.clone();
    // Tickets run beside reception and are cancelled/joined before return.
    let tickets = runtime.as_ref().and_then(|runtime| runtime.start_tickets());
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Fatal worker invalidation only: a cooperative shutdown must still drain
    // queued accepted work through the writer instead of discarding it.
    let worker_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let error = Arc::new(std::sync::Mutex::new(None));
    let worker_error = Arc::clone(&error);
    let (failed, failure) = tokio::sync::watch::channel(false);
    let receive_generation = Arc::clone(&generation);
    let automod_enabled = automod.is_some();
    // The worker dispatches text automations itself once automod has decided.
    let command_runtime = runtime.clone();
    let receive_state = Arc::clone(&state);
    let receive_onboarding = onboarding.clone();
    let in_flight_acks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let receive_pipeline = Arc::clone(&pipeline);
    let receive_voice = voice.clone();
    let events = futures_util::stream::unfold(
        (
            shard,
            saved,
            CHECKPOINT_IO_MAX,
            crate::gateway_metrics::Observer::default(),
            None::<tokio::sync::oneshot::Receiver<()>>,
        ),
        move |(mut shard, mut received, mut deadline, mut observer, mut bootstrap)| {
            let state = Arc::clone(&receive_state);
            let generation = Arc::clone(&receive_generation);
            let runtime = runtime.clone();
            let interactions = interactions.clone();
            let rsvp_sender = receive_rsvp.clone();
            let onboarding = receive_onboarding.clone();
            let in_flight_acks = Arc::clone(&in_flight_acks);
            let pipeline = Arc::clone(&receive_pipeline);
            let voice = receive_voice.clone();
            async move {
                // Failure is emitted to the ordered worker, never skipped past.
                let work: Result<Option<ReceivedWork>, sqlx::Error> = async {
                    if let Some(committed) = bootstrap.take() {
                        // A cold RESUME replayed missed dispatches but cannot
                        // populate a fresh cache. IDENTIFY only once RESUMED has
                        // committed; READY + GuildCreate then rebuild voice state.
                        if committed.await.is_err() {
                            return Ok(None);
                        }
                        transport_disconnected(&state, &generation).await;
                        voice_disconnected(voice.as_ref());
                        // Twilight consumes the boot session from config on
                        // construction; the config clone starts a fresh IDENTIFY.
                        shard = Shard::with_config(shard.id(), shard.config().clone());
                        info!("cold resume committed; requesting voice snapshot via identify");
                    }
                    while let Some(item) = shard.next().await {
                        let received_at = tokio::time::Instant::now();
                        let message = match item {
                            Ok(message) => message,
                            Err(error) if matches!(error.kind(), twilight_gateway::error::ReceiveMessageErrorType::Reconnect) => {
                                transport_disconnected(&state, &generation).await;
                                voice_disconnected(voice.as_ref());
                                warn!("gateway reconnect failed; Twilight will retry");
                                continue;
                            }
                            Err(_) => return Err(sqlx::Error::InvalidArgument("gateway receive failed".into())),
                        };
                        observer.observe(&message, &shard);
                        let Message::Text(text) = message else {
                            transport_disconnected(&state, &generation).await;
                            voice_disconnected(voice.as_ref());
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
                            voice_disconnected(voice.as_ref());
                            if invalidates_session(resumable) { received = None; return Ok(Some(ReceivedWork::Clear(deadline))); }
                        }
                        if header.op != 0 { continue; }
                        let sequence = header.s.ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing sequence".into()))?;
                        let session = session_snapshot(&shard).ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing session".into()))?;
                        let resume_url = shard.resume_url().or_else(|| received.as_ref().filter(|saved| saved.session_id == session.id()).map(|saved| saved.resume_url.as_str()))
                            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing resume URL".into()))?;
                        let checkpoint = GatewaySession { session_id: session.id().to_owned(), sequence, resume_url: resume_url.to_owned(), updated_at_ms: two_bot_core::funnel::now_millis_for_test() };
                        if dispatch_action(received.as_ref(), &checkpoint.session_id, sequence) == DispatchAction::Duplicate { continue; }
                        // A partial MESSAGE_UPDATE omits fields a full Twilight
                        // Message needs: decode its raw IDs before parsing.
                        let edit = if automod_enabled {
                            crate::automod_gateway::partial_edit(&text, crate::automod_gateway::receipt_ms(&observed_at))
                                .map(|delivery| Box::new((delivery, observed_at.clone())))
                        } else {
                            None
                        };
                        let parsed = match twilight_gateway::parse(text, EventTypeFlags::all()) {
                            Ok(parsed) => parsed,
                            Err(_) if edit.is_some() => None,
                            Err(_) => return Err(sqlx::Error::InvalidArgument("gateway dispatch parse failed".into())),
                        };
                        received = Some(checkpoint.clone());
                        let mut dispatch = parsed.map(|parsed| Box::new(ReceivedDispatch { event: Event::from(parsed), observed_at, completion: None }));
                        // Detached command ingress must not wait behind the
                        // serial funnel writer's REST/SQL latency. Main's
                        // command claims remain independent of this checkpoint.
                        // With automod active a create waits in the worker for its
                        // disposition: rejected creates never reach automations.
                        if let (Some(runtime), Some(dispatch)) = (runtime.as_ref(), dispatch.as_ref()) {
                            if !(automod_enabled && matches!(dispatch.event, Event::MessageCreate(_))) {
                                if interactions.is_some() {
                                    runtime.dispatch_remaining(&dispatch.event);
                                } else {
                                    runtime.dispatch(&dispatch.event);
                                }
                            }
                        }
                        // The ordered runtime is a separate instance from the
                        // command runtime's own copy: READY identity must reach
                        // it directly, or its application fence stays disarmed
                        // and bot-user checks stay lazy.
                        if let (Some(interactions), Some(dispatch)) =
                            (interactions.as_ref(), dispatch.as_ref())
                        {
                            if let Event::Ready(ready) = &dispatch.event {
                                interactions.set_bot_user_id(ready.user.id.get());
                                interactions.set_application_id(ready.application.id.get());
                            }
                        }
                        if let (Some(runtime), Some(sender), Some(dispatch)) = (interactions.as_ref(), rsvp_sender.as_ref(), dispatch.as_mut()) {
                            if let Event::InteractionCreate(interaction) = &dispatch.event {
                                let interaction = interaction.0.clone();
                                let runtime = Arc::clone(runtime);
                                let acknowledgement = tokio::spawn(async move { runtime.prepare(interaction).await });
                                let (completed, completion) = tokio::sync::oneshot::channel();
                                sender.send(AcceptedRsvp { acknowledgement, completed })
                                    .map_err(|_| sqlx::Error::InvalidArgument("interaction drain stopped".into()))?;
                                dispatch.completion = Some(completion);
                            }
                        }
                        // The only onboarding effect at reception is a bounded
                        // initial ACK via the shared executor; the memory-only
                        // ticket is consumed after the ordered COMMIT.
                        let acknowledgement = match (onboarding.as_ref(), dispatch.as_deref()) {
                            (Some(runtime), Some(ReceivedDispatch { event: Event::InteractionCreate(interaction), .. }))
                                if runtime.accepts_interaction(&interaction.0) =>
                            {
                                if in_flight_acks.fetch_add(1, Ordering::AcqRel) >= ACK_LIMIT {
                                    in_flight_acks.fetch_sub(1, Ordering::AcqRel);
                                    return Err(sqlx::Error::InvalidArgument("gateway ingress capacity exhausted".into()));
                                }
                                let (send, receive) = tokio::sync::oneshot::channel();
                                let runtime = Arc::clone(runtime);
                                let interaction = interaction.0.clone();
                                let in_flight = Arc::clone(&in_flight_acks);
                                tokio::spawn(async move {
                                    let confirmed = runtime.acknowledge(&interaction, received_at).await;
                                    in_flight.fetch_sub(1, Ordering::AcqRel);
                                    let _ = send.send(confirmed);
                                });
                                Some(receive)
                            }
                            _ => None,
                        };
                        // Only READY sets the current user, so a RESUMED without
                        // one is a cold resume across a process restart.
                        let mut committed = None;
                        if let (Some(voice), Some(dispatch)) = (voice.as_ref(), dispatch.as_ref()) {
                            if matches!(dispatch.event, Event::Resumed) && voice.needs_bootstrap(pipeline.cache()) {
                                let (sender, receiver) = tokio::sync::oneshot::channel();
                                committed = Some(sender);
                                bootstrap = Some(receiver);
                            }
                        }
                        return Ok(Some(ReceivedWork::Dispatch { dispatch, edit, checkpoint, deadline, generation: generation.load(std::sync::atomic::Ordering::Acquire), acknowledgement, committed }));
                    }
                    Ok(None)
                }.await;
                match work {
                    Ok(Some(work)) => {
                        Some((work, (shard, received, deadline, observer, bootstrap)))
                    }
                    Ok(None) => None,
                    Err(error) => Some((
                        ReceivedWork::Failed(error),
                        (shard, received, deadline, observer, bootstrap),
                    )),
                }
            }
        },
    );
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    let handle = tokio::runtime::Handle::current();
    let worker_state = Arc::clone(&state);
    let worker_voice = voice;
    let stop_state = Arc::clone(&state);
    let live_interactions: LiveInteractions = Arc::default();
    let queue_signal = Arc::new(tokio::sync::Notify::new());
    let writer_live = Arc::clone(&live_interactions);
    let writer_signal = Arc::clone(&queue_signal);
    let writer_onboarding = onboarding.clone();
    let queue_worker = onboarding.clone().map(|runtime| {
        onboarding_queue(
            runtime,
            store.clone(),
            Arc::clone(&state),
            Arc::clone(&generation),
            Arc::clone(&live_interactions),
            Arc::clone(&queue_signal),
        )
    });
    let dispatch = crate::dispatch::dispatch_bounded(
        // Ending reception is cooperative: dispatch_bounded keeps supervising
        // and draining its blocking writer instead of being aborted/dropped.
        events.take_until(async move {
            tokio::select! {
                _ = shutdown => {},
                _ = crate::server::shutdown_requested(failure) => {},
            }
        }),
        crate::dispatch::DISPATCH_BACKLOG,
        move |work| {
            // Already-running work may finish; queued funnel effects are not
            // admitted after a fatal worker error. A cooperative shutdown still
            // drains queued accepted work. Accepted RSVP has its own drain.
            if worker_failed.load(Ordering::Acquire) {
                return;
            }
            let operation =
                match work {
                    ReceivedWork::Clear(deadline) => handle.block_on(checkpoint_io(
                        &worker_state,
                        &generation,
                        deadline,
                        store.clear(),
                    )),
                    ReceivedWork::Failed(error) => Err(error),
                    ReceivedWork::Dispatch {
                        dispatch,
                        edit,
                        checkpoint,
                        deadline,
                        generation: observed_generation,
                        acknowledgement,
                        committed,
                    } => {
                        let timer = crate::gateway_metrics::DispatchTimer::start();
                        let mut connected = false;
                        let mut onboarding_job = None;
                        // Automod decides first, in gateway order, once per delivery.
                        let disposition = automod.as_ref().and_then(|automod| {
                            let (delivery, at) = match (edit, dispatch.as_deref()) {
                                (Some(edit), _) => *edit,
                                (None, Some(dispatch)) => (
                                    two_bot_discord::automod::event_to_automod(
                                        &dispatch.event,
                                        crate::automod_gateway::receipt_ms(&dispatch.observed_at),
                                    )?,
                                    dispatch.observed_at.clone(),
                                ),
                                (None, None) => return None,
                            };
                            Some(
                                handle.block_on(crate::automod_gateway::process(
                                    automod, delivery, &at,
                                )),
                            )
                        });
                        let mut acknowledgement_held = false;
                        if let Some(dispatch) = dispatch {
                            // A cold voice RESUME is followed by IDENTIFY; READY connects.
                            connected = matches!(dispatch.event, Event::Ready(_) | Event::Resumed)
                                && committed.is_none();
                            // Capture member state before the cache pipeline mutates it.
                            onboarding_job = writer_onboarding
                                .as_ref()
                                .and_then(|runtime| runtime.capture(&dispatch.event, &pipeline));
                            // Exactly one funnel call per dispatch, then drain deferred
                            // XP awards through the leveling runtime under the
                            // checkpoint deadline before the cursor commits. Without a
                            // leveling runtime the drain is a no-op.
                            let requests = match disposition {
                                Some(disposition) => pipeline.collect_at_with_message_disposition(
                                    &dispatch.event,
                                    &dispatch.observed_at,
                                    disposition,
                                ),
                                None => pipeline.collect_at(
                                    &dispatch.event,
                                    &dispatch.observed_at,
                                    two_bot_discord::MessageEligibility::default(),
                                ),
                            };
                            // Voice after the cache update, so snapshots are complete.
                            // Handling never blocks (actor inbox). A transport loss seen
                            // at reception after this dispatch must still win over any
                            // snapshot it just published.
                            if let Some(voice) = worker_voice.as_ref() {
                                voice.handle(&dispatch.event, pipeline.cache());
                                if generation.load(Ordering::Acquire) != observed_generation {
                                    voice.disconnect();
                                }
                            }
                            if automod_enabled
                                && matches!(dispatch.event, Event::MessageCreate(_))
                                && crate::automod_gateway::runs_text_automations(disposition)
                            {
                                if let Some(runtime) = command_runtime.as_ref() {
                                    // Detached spawn from the blocking worker needs the runtime.
                                    let _guard = handle.enter();
                                    runtime.dispatch(&dispatch.event);
                                }
                            }
                            if !requests.is_empty() {
                                handle
                            .block_on(checkpoint_io(&worker_state, &generation, deadline, async {
                                pipeline.drain(requests).await.map(drop).map_err(|error| {
                                    // Runtime Display is sanitized; never
                                    // log its SQL/HTTP source.
                                    tracing::warn!(
                                        error = %error,
                                        "gateway leveling dispatch failed"
                                    );
                                    sqlx::Error::InvalidArgument(
                                        "leveling gateway dispatch failed".into(),
                                    )
                                })
                            }))
                            .unwrap_or_else(|_| {
                                panic!("gateway leveling dispatch failed; checkpoint unchanged")
                            });
                            }
                            // A Blocked receipt-callback exhaustion leaves the command
                            // never-acknowledged: hold the cursor instead of
                            // silently passing it. The error path below releases
                            // a cold-resume wait and records the fence.
                            acknowledgement_held = if let Some(completion) = dispatch.completion {
                                !handle.block_on(completion).unwrap_or_else(|_| {
                                    panic!("interaction drain failed; checkpoint unchanged")
                                })
                            } else {
                                false
                            };
                        }
                        let durable_job = onboarding_job
                            .as_ref()
                            .map(|job| {
                                job.durable_payload().map(|payload| GatewayJob {
                                    payload,
                                    occurred_at_ms: checkpoint.updated_at_ms,
                                })
                            })
                            .transpose()
                            .unwrap_or_else(|_| panic!("invalid onboarding job"));
                        let checkpoint_result = if acknowledgement_held {
                            Err(sqlx::Error::InvalidArgument(
                                "interaction acknowledgement failed; checkpoint unchanged".into(),
                            ))
                        } else {
                            handle.block_on(checkpoint_io(
                                &worker_state,
                                &generation,
                                deadline,
                                store.commit_dispatch_with_job(
                                    &checkpoint,
                                    pipeline.handlers().store().take_batch(),
                                    durable_job,
                                ),
                            ))
                        };
                        // A failed checkpoint is recorded on `operation` (the
                        // worker stops and accepted RSVP drains) instead of
                        // panicking away accepted commands.
                        match checkpoint_result {
                            Ok((_, job_id)) => {
                                timer.committed();
                                if let Some(id) = job_id {
                                    if let Some(OnboardingJob::Interaction(interaction)) =
                                        onboarding_job
                                    {
                                        let ticket = acknowledgement.unwrap_or_else(|| {
                                            panic!("onboarding interaction missing ingress ticket")
                                        });
                                        writer_live
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .insert(
                                                id,
                                                LiveInteraction {
                                                    interaction,
                                                    ticket,
                                                    generation: observed_generation,
                                                },
                                            );
                                    }
                                    writer_signal.notify_one();
                                }
                                if connected {
                                    let mut state = handle.block_on(worker_state.write());
                                    if *state != GatewayState::Draining
                                        && generation.load(Ordering::Acquire) == observed_generation
                                    {
                                        *state = GatewayState::Connected;
                                        info!(
                                            sequence = checkpoint.sequence,
                                            "gateway ready; checkpoint committed"
                                        );
                                    }
                                }
                                if let Some(committed) = committed {
                                    let _ = committed.send(());
                                }
                                Ok(())
                            }
                            Err(error) => {
                                if let Some(committed) = committed {
                                    let _ = committed.send(());
                                }
                                Err(error)
                            }
                        }
                    }
                };
            if let Err(error) = operation {
                // Retain the original error without panicking away accepted
                // commands or allowing a later checkpoint to leap past failure.
                *worker_error.lock().expect("gateway error lock") = Some(error);
                worker_failed.store(true, Ordering::Release);
                failed.send_replace(true);
            }
        },
        move || async move {
            stopped.store(true, Ordering::Release);
            *stop_state.write().await = GatewayState::Draining;
        },
        crate::dispatch::DISPATCH_IO_MAX,
        crate::dispatch::DISPATCH_DRAIN_MAX,
    );
    let result: Result<(), sqlx::Error> = match queue_worker {
        // The durable queue worker only returns on a fatal error; fail the
        // runner (Draining) and drop reception/writer with it.
        Some(queue_worker) => tokio::select! {
            result = dispatch => {
                result.map_err(|reason| sqlx::Error::InvalidArgument(reason.into()))
            }
            result = queue_worker => {
                *state.write().await = GatewayState::Draining;
                result
            }
        },
        None => dispatch
            .await
            .map_err(|reason| sqlx::Error::InvalidArgument(reason.into())),
    };
    // Reception does not restart in this runner. Keep Draining sticky through
    // both successful shutdown and fatal exit, including any remaining writer.
    drop(rsvp_sender);
    let drained = match rsvp_drain {
        Some(task) => tokio::time::timeout(crate::dispatch::DISPATCH_DRAIN_MAX, task)
            .await
            .map_err(|_| sqlx::Error::InvalidArgument("interaction drain deadline exceeded".into()))
            .and_then(|result| {
                result.map_err(|_| sqlx::Error::InvalidArgument("interaction drain failed".into()))
            }),
        None => Ok(()),
    };
    if let Some(tickets) = tickets {
        tickets.shutdown().await;
    }
    drained?;
    // The dispatch supervisor is the authority on admission: when it fails
    // closed (backlog full, I/O or drain deadlines), its reason stands even
    // if the stuck worker later records its own checkpoint timeout. The
    // supervisor breaks first; the worker timeout is the consequence of the
    // same stuck checkpoint under drain, not a second cause. A cooperative
    // supervisor return still surfaces the worker's retained error, so
    // checkpoint-failure reporting after accepted-work drain is unchanged.
    result?;
    if let Some(error) = error.lock().expect("gateway error lock").take() {
        return Err(error);
    }
    Ok(())
}

/// Durable onboarding job worker: claims committed jobs, runs them through the
/// shared executor with bounded concurrency and records completion. Returns
/// only on a fatal error (the runner then fails closed).
async fn onboarding_queue(
    runtime: Arc<crate::onboarding::OnboardingRuntime>,
    store: GatewaySessionStore,
    state: Arc<RwLock<GatewayState>>,
    generation: Arc<AtomicU64>,
    live_interactions: LiveInteractions,
    signal: Arc<tokio::sync::Notify>,
) -> Result<(), sqlx::Error> {
    let deadline = CHECKPOINT_IO_MAX;
    let mut feature_jobs = tokio::task::JoinSet::new();
    let mut queue_dirty = true;
    let mut queue_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    queue_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    checkpoint_io(
        &state,
        &generation,
        deadline,
        store.recover_onboarding_jobs(),
    )
    .await?;
    loop {
        // Claim only available worker slots; reception keeps polling Twilight.
        if queue_dirty && feature_jobs.len() < ONBOARDING_WORKER_LIMIT {
            if let Some(saved) =
                checkpoint_io(&state, &generation, deadline, store.claim_onboarding_job()).await?
            {
                let live = live_interactions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&saved.id);
                let job = if let Some(live) = &live {
                    Some(OnboardingJob::Interaction(live.interaction.clone()))
                } else {
                    OnboardingJob::recover(&saved.payload).map_err(|_| {
                        sqlx::Error::InvalidArgument("invalid durable onboarding job".into())
                    })?
                };
                if let Some(job) = job {
                    let runtime = Arc::clone(&runtime);
                    let generation = Arc::clone(&generation);
                    feature_jobs.spawn(async move {
                        let interrupted =
                            tokio::time::timeout(std::time::Duration::from_secs(90), async {
                                if let Some(live) = live {
                                    // Invalidation cancels even settings/selection waits;
                                    // staged SQL rolls back and credentials are not replayed.
                                    tokio::select! {
                                        biased;
                                        _ = generation_changed(&generation, live.generation) => return Ok(true),
                                        result = async {
                                            if live.ticket.await.unwrap_or(false) {
                                                runtime.handle_acknowledged(&live.interaction, saved.occurred_at_ms).await
                                            } else {
                                                runtime.unconfirmed_interaction(&live.interaction).await
                                            }
                                        } => result?,
                                    }
                                } else {
                                    runtime.handle(job, saved.occurred_at_ms).await?;
                                }
                                Ok::<_, crate::onboarding::RuntimeError>(false)
                            })
                            .await
                            .map_err(|_| crate::onboarding::RuntimeError::Discord)??;
                        Ok::<_, crate::onboarding::RuntimeError>((saved.id, interrupted))
                    });
                } else {
                    // Callback credentials never survive a process boundary.
                    // Keep an interruption receipt, not a guessed role replay.
                    checkpoint_io(
                        &state,
                        &generation,
                        deadline,
                        store.finish_onboarding_job(saved.id, true),
                    )
                    .await?;
                    warn!(
                        job_id = saved.id,
                        "onboarding interaction interrupted; member must reselect"
                    );
                }
            } else {
                queue_dirty = false;
            }
        }
        tokio::select! {
            _ = signal.notified() => queue_dirty = true,
            _ = queue_tick.tick(), if queue_dirty && feature_jobs.len() < ONBOARDING_WORKER_LIMIT => {}
            result = feature_jobs.join_next(), if !feature_jobs.is_empty() => {
                let Some(Ok(Ok((id, interrupted)))) = result else {
                    return Err(sqlx::Error::InvalidArgument(
                        "onboarding worker failed; durable job retained for bounded restart recovery".into(),
                    ));
                };
                checkpoint_io(&state, &generation, deadline, store.finish_onboarding_job(id, interrupted)).await?;
                queue_dirty = true;
            }
        }
    }
}

/// Build the supervisor's shard: single-shard deployment (one guild, ADR
/// 0001) over [`intents_from_env`].
///
/// A stored [`Session`] (S5) resumes the previous gateway session instead of
/// a fresh IDENTIFY.
#[must_use]
pub fn build_shard(
    token: String,
    intents: Intents,
    session: Option<&GatewaySession>,
    gateway_url: Option<&str>,
) -> Shard {
    let config = build_shard_config(token, intents, session);
    let config = match gateway_url {
        Some(url) => twilight_gateway::ConfigBuilder::from(config)
            .proxy_url(url.to_owned())
            .build(),
        None => config,
    };
    Shard::with_config(ShardId::ONE, config)
}

/// The opt-in binary acceptance seam must never send a token to a remote host.
/// Accept literal loopback sockets only; no DNS, credentials, paths or queries.
pub fn is_loopback_gateway(url: &str) -> bool {
    url.strip_prefix("ws://")
        .and_then(|socket| socket.parse::<std::net::SocketAddr>().ok())
        .is_some_and(|socket| socket.ip().is_loopback() && socket.port() != 0)
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
pub fn build_pipeline(
    milestones: Vec<two_bot_core::FunnelEvent>,
    runtime: Option<LevelingRuntime>,
) -> GatewayPipeline {
    let buffer = GatewayFunnelBuffer::from_milestones(milestones);
    OrderedLevelingPipeline::with_snapshots(
        buffer.clone(),
        runtime,
        two_bot_discord::NoInvites,
        buffer,
    )
}

pub async fn build_persistent_pipeline(
    store: &GatewaySessionStore,
    guild_id: Snowflake,
    token: String,
    leveling: Option<LevelingRuntime>,
) -> Result<GatewayPipeline<HttpInvites>, sqlx::Error> {
    tokio::time::timeout(CHECKPOINT_IO_MAX, async {
        let buffer = GatewayFunnelBuffer::from_milestones(store.milestones().await?);
        buffer.seed_snapshots(guild_id, store.invite_snapshots().await?);
        Ok(OrderedLevelingPipeline::with_snapshots(
            buffer.clone(),
            leveling,
            HttpInvites {
                client: twilight_http::Client::new(token),
                handle: tokio::runtime::Handle::current(),
            },
            buffer,
        ))
    })
    .await
    .map_err(|_| sqlx::Error::InvalidArgument("gateway baseline deadline exceeded".into()))?
}

/// Build the V1 voice sink, or `None` when voice stays off.
///
/// Voice needs all three: the `TWO_VOICE=1` gate, a Discord token
/// (single-attempt REST), and a Postgres URL for the pre-migrated sqlx store.
/// Anything missing — or any construction failure — degrades to voice-off
/// with a warn; the gateway and /readyz keep working. Secrets never appear
/// in the logs.
pub async fn build_voice_runtime(
    config: &Config,
    voice_enabled: bool,
) -> Option<Arc<dyn VoiceEventSink>> {
    if !voice_enabled {
        return None;
    }
    let token = match config
        .discord_token
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|token| !token.is_empty())
    {
        Some(token) => token,
        None => {
            warn!("TWO_VOICE=1 but no discord token; voice rooms disabled");
            return None;
        }
    };
    let database_url = match config
        .database_url
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|url| !url.is_empty())
    {
        Some(url) => url,
        None => {
            warn!("TWO_VOICE=1 but no database URL; voice rooms disabled");
            return None;
        }
    };
    // Migrations belong to the operator's migrator role, never the DML-only
    // runtime credential. Voice consumes the already-migrated schema.
    let db = match connect(database_url, DB_POOL_MAX_DEFAULT, true).await {
        Ok(db) => db,
        Err(_) => {
            warn!("voice database unavailable; voice rooms disabled");
            return None;
        }
    };
    match build_production_runtime(token, db.pool().clone()) {
        Ok(runtime) => {
            info!("voice rooms enabled; gateway sink attached");
            Some(Arc::new(runtime))
        }
        Err(err) => {
            warn!(error = %err, "voice HTTP setup failed; voice rooms disabled");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_gateway_override_accepts_literal_loopback_only() {
        for url in ["ws://127.0.0.1:1234", "ws://[::1]:1234"] {
            assert!(is_loopback_gateway(url));
        }
        for url in [
            "ws://discord.com:443",
            "wss://127.0.0.1:443",
            "ws://192.0.2.1:1234",
            "ws://localhost:1234",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1:1234/path",
            "ws://user@127.0.0.1:1234",
            "ws://127.0.0.1:1234?host=discord.com",
            "ws://[::ffff:192.0.2.1]:1234",
            "",
        ] {
            assert!(!is_loopback_gateway(url), "must reject {url}");
        }
    }

    fn configured() -> Config {
        Config {
            discord_token: Some(two_bot_core::Secret::new("token".to_owned())),
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
        let shard = build_shard("token".to_owned(), Intents::empty(), None, None);
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

    fn voice_config(token: Option<&str>, database_url: Option<&str>) -> Config {
        Config {
            discord_token: token.map(|value| two_bot_core::Secret::new(value.to_owned())),
            database_url: database_url.map(|value| two_bot_core::Secret::new(value.to_owned())),
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        }
    }

    #[tokio::test]
    async fn voice_runtime_off_without_gate() {
        let config = voice_config(Some("token"), Some("postgres://localhost/unused"));
        assert!(build_voice_runtime(&config, false).await.is_none());
    }

    #[tokio::test]
    async fn voice_runtime_off_without_token() {
        let config = voice_config(None, Some("postgres://localhost/unused"));
        assert!(build_voice_runtime(&config, true).await.is_none());
    }

    #[tokio::test]
    async fn voice_runtime_off_without_database() {
        let config = voice_config(Some("token"), None);
        assert!(build_voice_runtime(&config, true).await.is_none());
    }

    #[tokio::test]
    async fn voice_runtime_off_on_bad_database_url() {
        #[derive(Clone)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let capture = Capture(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        // Fails at URL validation: no socket is ever opened. Diagnostics must
        // remain useful without exposing the rejected credential-bearing value.
        let url = "fixture-invalid-scheme://fixture-user:fixture-db-password@agent-testdb/db";
        let config = voice_config(Some("fixture-discord-token"), Some(url));
        assert!(build_voice_runtime(&config, true).await.is_none());
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("voice database unavailable; voice rooms disabled"));
        for secret in [
            url,
            "fixture-user",
            "fixture-db-password",
            "fixture-invalid-scheme",
            "fixture-discord-token",
        ] {
            assert!(!output.contains(secret));
        }
    }
}
