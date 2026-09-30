//! Shared async reply lifecycle. The transport owns Discord I/O; feature
//! handlers keep their existing signatures and are adapted with a closure.

use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration};

use futures_util::FutureExt;
use tokio::sync::Mutex;

/// Reply text for stale or unknown commands, components and modals.
pub const UNKNOWN_INTERACTION_REPLY: &str = "This interaction is no longer available.";

/// A handler's completed text reply. Visibility is fixed by the first ACK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionReply {
    pub content: String,
    pub ephemeral: bool,
}

impl InteractionReply {
    #[must_use]
    pub fn new(content: impl Into<String>, ephemeral: bool) -> Self {
        // Discord's content ceiling; never split a Unicode scalar.
        Self {
            content: content.into().chars().take(2000).collect(),
            ephemeral,
        }
    }
}

/// Framework-free operations translated by the Discord adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOperation {
    Respond(InteractionReply),
    Defer { ephemeral: bool },
    EditOriginal { content: String },
    Followup(InteractionReply),
    DeleteOriginal,
}

/// One interaction's transport. Never include its token in Debug/log output.
#[derive(Debug, thiserror::Error)]
pub enum ReplyError<E: std::error::Error> {
    #[error("interaction response transport failed")]
    Transport(#[source] E),
    #[error("an earlier response failed; its delivery is uncertain")]
    DeliveryUncertain,
}

pub trait ReplyTransport: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn execute(
        &self,
        operation: ReplyOperation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Time to start the deferred ACK, leaving headroom before Discord's 3 s limit.
#[derive(Debug, Clone, Copy)]
pub struct ReplyPolicy {
    budget: Duration,
}

impl Default for ReplyPolicy {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(2),
        }
    }
}

impl ReplyPolicy {
    /// A zero budget defers immediately. Budgets at/over Discord's deadline
    /// are rejected; the caller must also allow for network latency.
    pub fn new(budget: Duration) -> Result<Self, &'static str> {
        if budget >= Duration::from_secs(3) {
            return Err("interaction reply budget must be less than 3 seconds");
        }
        Ok(Self { budget })
    }

    #[must_use]
    pub fn budget(self) -> Duration {
        self.budget
    }
}

#[derive(Debug, Clone, Copy)]
enum ReplyState {
    Unanswered,
    Deferred { ephemeral: bool },
    Replied,
    // A failed ACK may have reached Discord. Do not send a second callback.
    Failed,
}

/// Serialized response state shared by the handler and the deadline wrapper.
/// Early replies/defer calls must go through this session, not raw HTTP.
pub struct ReplySession<'a, T> {
    transport: &'a T,
    state: Arc<Mutex<ReplyState>>,
}

impl<T> Clone for ReplySession<'_, T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport,
            state: Arc::clone(&self.state),
        }
    }
}

impl<'a, T: ReplyTransport> ReplySession<'a, T> {
    fn new(transport: &'a T) -> Self {
        Self {
            transport,
            state: Arc::new(Mutex::new(ReplyState::Unanswered)),
        }
    }

    /// ACK only if neither the handler nor the deadline has already done so.
    pub async fn defer(&self, ephemeral: bool) -> Result<(), ReplyError<T::Error>> {
        let mut state = self.state.lock().await;
        if matches!(*state, ReplyState::Failed) {
            return Err(ReplyError::DeliveryUncertain);
        }
        if matches!(*state, ReplyState::Unanswered) {
            *state = ReplyState::Failed;
            self.transport
                .execute(ReplyOperation::Defer { ephemeral })
                .await
                .map_err(ReplyError::Transport)?;
            *state = ReplyState::Deferred { ephemeral };
        }
        Ok(())
    }

    /// An early reply or the handler's completion: callback once, then edit.
    pub async fn respond(&self, reply: InteractionReply) -> Result<(), ReplyError<T::Error>> {
        let mut state = self.state.lock().await;
        let reply = InteractionReply::new(reply.content, reply.ephemeral);
        let operation = match *state {
            ReplyState::Unanswered => ReplyOperation::Respond(reply),
            ReplyState::Deferred { ephemeral: false } if reply.ephemeral => {
                *state = ReplyState::Failed;
                self.transport
                    .execute(ReplyOperation::DeleteOriginal)
                    .await
                    .map_err(ReplyError::Transport)?;
                ReplyOperation::Followup(reply)
            }
            ReplyState::Deferred { .. } | ReplyState::Replied => ReplyOperation::EditOriginal {
                content: reply.content,
            },
            ReplyState::Failed => return Err(ReplyError::DeliveryUncertain),
        };
        *state = ReplyState::Failed;
        self.transport
            .execute(operation)
            .await
            .map_err(ReplyError::Transport)?;
        *state = ReplyState::Replied;
        Ok(())
    }

    async fn fail(&self, content: String) -> Result<(), ReplyError<T::Error>> {
        let mut state = self.state.lock().await;
        let previous = *state;
        *state = ReplyState::Failed;
        let reply = InteractionReply::new(content, true);
        match previous {
            ReplyState::Unanswered => self
                .transport
                .execute(ReplyOperation::Respond(reply))
                .await
                .map_err(ReplyError::Transport)?,
            ReplyState::Deferred { ephemeral: true } => {
                self.transport
                    .execute(ReplyOperation::EditOriginal {
                        content: reply.content,
                    })
                    .await
                    .map_err(ReplyError::Transport)?;
            }
            ReplyState::Deferred { ephemeral: false } => {
                // Discord cannot change visibility on an edit. Remove the
                // public loading placeholder, then create a private followup.
                self.transport
                    .execute(ReplyOperation::DeleteOriginal)
                    .await
                    .map_err(ReplyError::Transport)?;
                self.transport
                    .execute(ReplyOperation::Followup(reply))
                    .await
                    .map_err(ReplyError::Transport)?;
            }
            ReplyState::Replied => self
                .transport
                .execute(ReplyOperation::Followup(reply))
                .await
                .map_err(ReplyError::Transport)?,
            ReplyState::Failed => return Err(ReplyError::DeliveryUncertain),
        }
        *state = ReplyState::Replied;
        Ok(())
    }
}

/// Run an adapted handler without spawning/detaching it. Both future creation
/// and polling panics are isolated. Requires panic=unwind (including release).
/// Handlers must yield: blocking CPU/I/O cannot be preempted by an async timer.
/// `ephemeral` is the command's visibility policy, known before it executes.
/// Transport errors propagate; callbacks are never blindly retried.
pub async fn run_handler<'a, T, H, F, E>(
    transport: &'a T,
    policy: ReplyPolicy,
    ephemeral: bool,
    handler: H,
) -> Result<(), ReplyError<T::Error>>
where
    T: ReplyTransport,
    H: FnOnce(ReplySession<'a, T>) -> F,
    F: Future<Output = Result<InteractionReply, E>>,
    E: Debug,
{
    let session = ReplySession::new(transport);
    let deadline = tokio::time::sleep(policy.budget());
    let result = match std::panic::catch_unwind(AssertUnwindSafe(|| handler(session.clone()))) {
        Ok(future) => {
            let future = AssertUnwindSafe(future).catch_unwind();
            tokio::pin!(future);
            tokio::pin!(deadline);
            tokio::select! {
                biased;
                result = &mut future => result,
                _ = &mut deadline => {
                    // Keep polling the handler while acquiring/sending the ACK:
                    // an early handler reply may itself hold the session lock.
                    // Wait for an in-flight defer before completing its edit.
                    let (_, result) = tokio::try_join!(
                        session.defer(ephemeral),
                        async { Ok::<_, ReplyError<T::Error>>(future.await) },
                    )?;
                    result
                }
            }
        }
        Err(panic) => Err(panic),
    };
    match result {
        Ok(Ok(reply)) => session.respond(reply).await,
        failure => {
            // Eight hex digits, independent of user data/interaction tokens.
            let reference = format!("{:08X}", rand::random::<u32>());
            match failure {
                Ok(Err(error)) => tracing::error!(%reference, ?error, "interaction handler failed"),
                Err(panic) => {
                    let error = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic");
                    tracing::error!(%reference, error, "interaction handler panicked");
                }
                Ok(Ok(_)) => unreachable!(),
            }
            session
                .fail(format!("Something went wrong (ref {reference})"))
                .await
        }
    }
}
