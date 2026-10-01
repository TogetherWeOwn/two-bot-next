//! Bounded, SQL-free shard polling; ordered persistence remains in the owner.
use super::*;
use futures_util::StreamExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot, watch};

pub(super) const CAPACITY: usize = 32;

pub(super) enum Packet {
    Hello(std::time::Duration),
    Invalidate {
        clear: bool,
    },
    Dispatch {
        event: Option<Box<Event>>,
        checkpoint: GatewaySession,
        generation: u64,
        acknowledgement: Option<oneshot::Receiver<bool>>,
    },
}

async fn invalidate(state: &RwLock<GatewayState>, generation: &AtomicU64) {
    let mut state = state.write().await;
    generation.fetch_add(1, Ordering::SeqCst);
    *state = GatewayState::Armed;
}

fn enqueue(sender: &mpsc::Sender<Packet>, packet: Packet) -> Result<(), sqlx::Error> {
    // Never wait for SQL to free buffer space: exhaustion is an essential-runner
    // failure, not an unbounded backlog or silently discarded dispatch.
    sender.try_send(packet).map_err(|_| {
        sqlx::Error::InvalidArgument("gateway ingress capacity exhausted or owner stopped".into())
    })
}

pub(super) async fn run(
    shard: &mut Shard,
    state: &RwLock<GatewayState>,
    generation: &AtomicU64,
    onboarding: Option<&Arc<crate::onboarding::OnboardingRuntime>>,
    sender: mpsc::Sender<Packet>,
    saved: watch::Receiver<Option<GatewaySession>>,
) -> Result<(), sqlx::Error> {
    let mut observer = crate::gateway_metrics::Observer::default();
    let mut acknowledgements = tokio::task::JoinSet::new();
    let mut received = shard.session().map(|session| GatewaySession {
        session_id: session.id().to_owned(),
        sequence: session.sequence(),
        resume_url: shard.resume_url().unwrap_or_default().to_owned(),
        updated_at_ms: 0,
    });
    loop {
        let item = tokio::select! {
            item = shard.next() => item,
            result = acknowledgements.join_next(), if !acknowledgements.is_empty() => {
                if !matches!(result, Some(Ok(()))) {
                    return Err(sqlx::Error::InvalidArgument("gateway acknowledgement task failed".into()));
                }
                continue;
            }
        };
        let Some(item) = item else { break };
        let received_at = tokio::time::Instant::now();
        let occurred_at_ms = two_bot_core::funnel::now_millis_for_test();
        let message = match item {
            Ok(message) => message,
            Err(error)
                if matches!(
                    error.kind(),
                    twilight_gateway::error::ReceiveMessageErrorType::Reconnect
                ) =>
            {
                invalidate(state, generation).await;
                enqueue(&sender, Packet::Invalidate { clear: false })?;
                warn!("gateway reconnect failed; Twilight will retry");
                continue;
            }
            Err(_) => {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway receive failed; checkpoint unchanged".into(),
                ))
            }
        };
        observer.observe(&message, shard);
        let Message::Text(text) = message else {
            invalidate(state, generation).await;
            // Discord rejects these retained Twilight sessions. Rebuild using
            // the already-consumed config so the replacement IDENTIFYs.
            let rejected = matches!(message, Message::Close(Some(ref frame)) if matches!(frame.code, 4007 | 4009));
            let clear = rejected || shard.session().is_none();
            enqueue(&sender, Packet::Invalidate { clear })?;
            if clear {
                received = None;
            }
            if rejected {
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
            enqueue(
                &sender,
                Packet::Hello(
                    CHECKPOINT_IO_MAX
                        .min(std::time::Duration::from_millis(hello.d.heartbeat_interval) / 4),
                ),
            )?;
        }
        if header.op == 9 {
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                sqlx::Error::InvalidArgument("invalid gateway session packet".into())
            })?;
            let resumable = value["d"].as_bool().ok_or_else(|| {
                sqlx::Error::InvalidArgument("invalid gateway session flag".into())
            })?;
            invalidate(state, generation).await;
            let clear = invalidates_session(resumable);
            enqueue(&sender, Packet::Invalidate { clear })?;
            if clear {
                received = None;
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
        // Clone under the watch borrow: no non-Send read guard crosses a poll.
        let persisted = saved.borrow().clone();
        let prior = received.as_ref().or(persisted.as_ref());
        if dispatch_action(prior, session.id(), sequence) == DispatchAction::Duplicate {
            continue;
        }
        let resume_url = shard
            .resume_url()
            .or_else(|| {
                prior
                    .filter(|prior| prior.session_id == session.id())
                    .map(|prior| prior.resume_url.as_str())
                    .filter(|url| !url.is_empty())
            })
            .or_else(|| {
                persisted
                    .as_ref()
                    .filter(|prior| prior.session_id == session.id())
                    .map(|prior| prior.resume_url.as_str())
            })
            .ok_or_else(|| sqlx::Error::InvalidArgument("dispatch missing resume URL".into()))?;
        let checkpoint = GatewaySession {
            session_id: session.id().to_owned(),
            sequence,
            resume_url: resume_url.to_owned(),
            updated_at_ms: occurred_at_ms,
        };
        drop(persisted);
        let event = twilight_gateway::parse(text, EventTypeFlags::all())
            .map_err(|_| {
                sqlx::Error::InvalidArgument(
                    "gateway dispatch parse failed; checkpoint unchanged".into(),
                )
            })?
            .map(Event::from)
            .map(Box::new);
        // The only ingress effect is a bounded initial ACK via the shared
        // executor. The memory-only ticket is consumed after ordered COMMIT.
        let acknowledgement = match (onboarding, event.as_deref()) {
            (Some(runtime), Some(Event::InteractionCreate(interaction)))
                if runtime.accepts_interaction(&interaction.0) =>
            {
                if acknowledgements.len() >= CAPACITY {
                    return Err(sqlx::Error::InvalidArgument(
                        "gateway acknowledgement capacity exhausted".into(),
                    ));
                }
                let (send, receive) = oneshot::channel();
                let runtime = Arc::clone(runtime);
                let interaction = interaction.0.clone();
                acknowledgements.spawn(async move {
                    let confirmed = runtime.acknowledge(&interaction, received_at).await;
                    let _ = send.send(confirmed);
                });
                Some(receive)
            }
            _ => None,
        };
        received = Some(checkpoint.clone());
        enqueue(
            &sender,
            Packet::Dispatch {
                event,
                checkpoint,
                generation: generation.load(Ordering::SeqCst),
                acknowledgement,
            },
        )?;
    }
    // Stream termination or owner cancellation drops this JoinSet, aborting
    // pending callbacks rather than detaching credential-bearing tasks.
    warn!("gateway shard stream ended; supervisor reports down until restart");
    Ok(())
}
