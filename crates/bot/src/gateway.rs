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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod ingress;

use crate::onboarding::OnboardingJob;

use tokio::sync::RwLock;
use tracing::{info, warn};
use twilight_gateway::{Event, EventTypeFlags, Intents, Message, Session, Shard, ShardId};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{
    boot_action, dispatch_action, invalidates_session, BootAction, DispatchAction, GatewaySession,
};
use two_bot_core::{ComponentStatus, Config, FunnelEvent};
use two_bot_cutover::gateway_session::{GatewayJob, GatewaySessionStore};
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

// Partition the existing five-connection gateway subsystem budget. These must
// be distinct pools: feature transactions retain connections across Discord I/O.
pub const GATEWAY_POOL_MAX: u32 = 1;
pub const FEATURE_POOL_MAX: u32 = two_bot_cutover::DB_POOL_MAX_DEFAULT - GATEWAY_POOL_MAX;
// Admission is independent of the 32-row durable queue. Leave feature capacity
// for settings and shared commands; do not claim more jobs while these workers run.
const ONBOARDING_WORKER_LIMIT: usize = 2;

const CHECKPOINT_IO_MAX: std::time::Duration = std::time::Duration::from_secs(5);

struct LiveInteraction {
    interaction: Box<twilight_model::application::interaction::Interaction>,
    ticket: tokio::sync::oneshot::Receiver<bool>,
    generation: u64,
}

async fn generation_changed(generation: &AtomicU64, expected: u64) {
    while generation.load(Ordering::SeqCst) == expected {
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

/// Bound the entire SQL operation (pool acquire through COMMIT), not each query.
/// Retain the quarter-heartbeat SQL budget even with independent ingress.
/// A transport invalidation during SQL must not restore stale readiness.
/// Source: <https://docs.rs/tokio/1/tokio/time/fn.timeout.html>
async fn checkpoint_io<T>(
    state: &RwLock<GatewayState>,
    generation: &AtomicU64,
    deadline: std::time::Duration,
    operation: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    let started_generation = generation.load(Ordering::SeqCst);
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
        let mut state = state.write().await;
        if generation.load(Ordering::SeqCst) == started_generation {
            *state = previous;
        }
    }
    result
}

/// Drive raw packets so even dispatches not mapped by Twilight have a durable
/// sequence. Twilight itself still owns transport, heartbeat and opcode-9
/// fallback. Source: <https://docs.rs/twilight-gateway/0.17.1/twilight_gateway/struct.Shard.html>
///
/// `runtime` is the shared command runtime (TOG-11020; S4 sticky slice was
/// TOG-10309): `dispatch` spawns detached work. Independent ingress drives
/// Twilight and initial onboarding ACKs while the ordered owner captures and
/// commits dispatches; command REST work never blocks either owner.
pub async fn run_shard(
    mut shard: Shard,
    pipeline: Arc<GatewayPipeline>,
    state: Arc<RwLock<GatewayState>>,
    store: GatewaySessionStore,
    onboarding: Option<Arc<crate::onboarding::OnboardingRuntime>>,
    runtime: Option<Arc<crate::command_runtime::CommandRuntime>>,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), sqlx::Error> {
    let tickets = runtime.as_ref().and_then(|runtime| runtime.start_tickets());
    let (sender, mut packets) = tokio::sync::mpsc::channel(ingress::CAPACITY);
    let (saved, checkpoint) = tokio::sync::watch::channel(None);
    let generation = Arc::new(AtomicU64::new(0));
    // Both futures are cancellation-owned by the essential runner. SQL never
    // prevents ingress polling; failure drops ACK/feature JoinSets together.
    let work = async {
        tokio::try_join!(
            ingress::run(
                &mut shard,
                &state,
                &generation,
                onboarding.as_ref(),
                sender,
                checkpoint,
            ),
            run_loop(
                &mut packets,
                &pipeline,
                &state,
                &store,
                onboarding.as_ref(),
                runtime.as_ref(),
                (&generation, saved),
            ),
        )
        .map(|_| ())
    };
    let result = tokio::select! {
        biased;
        _ = async {
            match shutdown {
                Some(receiver) => crate::server::shutdown_requested(receiver).await,
                None => std::future::pending().await,
            }
        } => Ok(()),
        result = work => result,
    };
    *state.write().await = GatewayState::Armed;
    if let Some(tickets) = tickets {
        tickets.shutdown().await;
    }
    result
}

async fn run_loop(
    packets: &mut tokio::sync::mpsc::Receiver<ingress::Packet>,
    pipeline: &GatewayPipeline,
    state: &RwLock<GatewayState>,
    store: &GatewaySessionStore,
    onboarding: Option<&Arc<crate::onboarding::OnboardingRuntime>>,
    runtime: Option<&Arc<crate::command_runtime::CommandRuntime>>,
    progress: (
        &Arc<AtomicU64>,
        tokio::sync::watch::Sender<Option<GatewaySession>>,
    ),
) -> Result<(), sqlx::Error> {
    let (generation, saved_checkpoint) = progress;
    let mut deadline = CHECKPOINT_IO_MAX;
    let mut committed = checkpoint_io(state, generation, deadline, store.load()).await?;
    saved_checkpoint.send_replace(committed.clone());
    info!(shard = ?ShardId::ONE, "gateway shard loop started");
    let mut feature_jobs = tokio::task::JoinSet::new();
    let mut live_interactions: HashMap<i64, LiveInteraction> = HashMap::new();
    let mut queue_dirty = true;
    let mut queue_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    queue_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    if onboarding.is_some() {
        checkpoint_io(state, generation, deadline, store.recover_onboarding_jobs()).await?;
    }
    loop {
        if let Some(runtime) = onboarding {
            // Claim only available worker slots and drain packets between
            // claims; independent ingress continues polling Twilight.
            if queue_dirty && feature_jobs.len() < ONBOARDING_WORKER_LIMIT {
                if let Some(saved) =
                    checkpoint_io(state, generation, deadline, store.claim_onboarding_job()).await?
                {
                    let live = live_interactions.remove(&saved.id);
                    let job = if let Some(live) = &live {
                        Some(OnboardingJob::Interaction(live.interaction.clone()))
                    } else {
                        OnboardingJob::recover(&saved.payload).map_err(|_| {
                            sqlx::Error::InvalidArgument("invalid durable onboarding job".into())
                        })?
                    };
                    if let Some(job) = job {
                        let runtime = Arc::clone(runtime);
                        let generation = Arc::clone(generation);
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
                            state,
                            generation,
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
        }
        let item = tokio::select! {
            item = packets.recv() => item,
            _ = queue_tick.tick(), if onboarding.is_some() && queue_dirty && feature_jobs.len() < ONBOARDING_WORKER_LIMIT => {
                continue;
            },
            result = feature_jobs.join_next(), if !feature_jobs.is_empty() => {
                let Some(Ok(Ok((id, interrupted)))) = result else {
                    return Err(sqlx::Error::InvalidArgument(
                        "onboarding worker failed; durable job retained for bounded restart recovery".into(),
                    ));
                };
                checkpoint_io(state, generation, deadline, store.finish_onboarding_job(id, interrupted)).await?;
                queue_dirty = true;
                continue;
            }
        };
        let Some(item) = item else {
            break;
        };
        let (event, checkpoint, received_generation, acknowledgement) = match item {
            ingress::Packet::Hello(budget) => {
                deadline = budget;
                continue;
            }
            ingress::Packet::Invalidate { clear } => {
                if clear {
                    checkpoint_io(state, generation, deadline, store.clear()).await?;
                    committed = None;
                    saved_checkpoint.send_replace(None);
                }
                continue;
            }
            ingress::Packet::Dispatch {
                event,
                checkpoint,
                generation,
                acknowledgement,
            } => (event, checkpoint, generation, acknowledgement),
        };
        let sequence = checkpoint.sequence;
        if dispatch_action(committed.as_ref(), &checkpoint.session_id, sequence)
            == DispatchAction::Duplicate
        {
            continue;
        }
        let timer = crate::gateway_metrics::DispatchTimer::start();
        let mut connected = false;
        let mut onboarding_job = None;
        if let Some(event) = event {
            connected = matches!(*event, Event::Ready(_) | Event::Resumed);
            onboarding_job = onboarding.and_then(|runtime| runtime.capture(&event, pipeline));
            pipeline.handle(&event);
            // Detached dispatch only: command REST work must not stall the
            // ordered pipeline/checkpoint owner.
            if let Some(runtime) = runtime {
                runtime.dispatch(&event);
            }
        }
        let durable_job = onboarding_job
            .as_ref()
            .map(|job| {
                job.durable_payload()
                    .map(|payload| GatewayJob {
                        payload,
                        occurred_at_ms: checkpoint.updated_at_ms,
                    })
                    .map_err(|_| sqlx::Error::InvalidArgument("invalid onboarding job".into()))
            })
            .transpose()?;
        let (_, job_id) = checkpoint_io(
            state,
            generation,
            deadline,
            store.commit_dispatch_with_job(
                &checkpoint,
                pipeline.handlers().store().take_batch(),
                durable_job,
            ),
        )
        .await?;
        timer.committed();
        committed = Some(checkpoint);
        saved_checkpoint.send_replace(committed.clone());
        if let Some(id) = job_id {
            if let Some(OnboardingJob::Interaction(interaction)) = onboarding_job {
                let ticket = acknowledgement.ok_or_else(|| {
                    sqlx::Error::InvalidArgument(
                        "onboarding interaction missing ingress ticket".into(),
                    )
                })?;
                live_interactions.insert(
                    id,
                    LiveInteraction {
                        interaction,
                        ticket,
                        generation: received_generation,
                    },
                );
            }
            queue_dirty = true;
        }
        if connected {
            let mut state = state.write().await;
            if generation.load(Ordering::SeqCst) == received_generation {
                *state = GatewayState::Connected;
                info!(sequence, "gateway ready; checkpoint committed");
            }
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
    async fn checkpoint_io_cannot_restore_an_invalidated_transport_generation() {
        let state = RwLock::new(GatewayState::Connected);
        let generation = AtomicU64::new(0);
        checkpoint_io(&state, &generation, CHECKPOINT_IO_MAX, async {
            generation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(*state.read().await, GatewayState::Armed);
    }

    #[tokio::test]
    async fn fresh_shard_has_no_session_to_persist() {
        ensure_crypto_provider();
        let shard = build_shard("token".to_owned(), Intents::empty(), None, None);
        assert_eq!(session_snapshot(&shard), None);
    }
}
