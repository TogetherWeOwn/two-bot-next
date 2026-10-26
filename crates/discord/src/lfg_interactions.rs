//! LFG interaction handlers using the shared router, store and REST executor.

use crate::{automation_admission::ActorCooldowns, ActionExecutor, CloseRefreshEdit, DiscordError};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use two_bot_core::lfg::{self, LfgPost, LfgRole, LfgSelectAction, LfgSignup, LfgStatus};
use two_bot_core::{lfg_store as store, rsvp::RsvpAudit, rsvp_store};

#[derive(Debug)]
pub enum LfgRequest {
    Create {
        title: String,
        starts_at: String,
        roles: String,
        channel_id: String,
    },
    /// Private `/lfg preview:true` render: validated and rendered with the
    /// same framework-free core as [`LfgRequest::Create`], but answered
    /// before admission with no pool, identity, audit or channel effect.
    Preview {
        title: String,
        starts_at: String,
        roles: String,
    },
    Close {
        post_id: String,
    },
    Edit {
        post_id: String,
        title: Option<String>,
        starts_at: Option<String>,
    },
    Select(LfgSelectAction),
}

#[derive(Debug, thiserror::Error)]
pub enum LfgError {
    #[error("{0}")]
    Invalid(String),
    #[error("LFG database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("LFG audit operation failed")]
    Audit(#[from] rsvp_store::RsvpStoreError),
    #[error("LFG Discord operation failed")]
    Discord(#[from] DiscordError),
    #[error(transparent)]
    Definition(#[from] two_bot_core::automation_quota::QuotaWriteError),
    #[error("LFG acceptance is uncertain; durable state retained for nonce recovery")]
    Uncertain,
    #[error("LFG is busy right now; try again in a few seconds.")]
    Busy,
    #[error("Wait 5 seconds before joining or changing another LFG role.")]
    CoolingDown,
}

/// LFG executions allowed to hold a pool connection at once, per service.
///
/// Each execution keeps its advisory-lock transaction open across paced REST
/// and takes one more connection for every store call. With the production
/// pool of 5 (`two_bot_store::pool::DB_POOL_MAX`), one in-flight execution uses
/// at most 2, so the gateway checkpoint writer and the other features always
/// find a free connection. Waiters queue here before `begin()`, holding none.
pub const LFG_MAX_IN_FLIGHT: usize = 1;

/// Executions allowed to queue behind the running one, per process.
///
/// Every queued execution occupies one slot of the gateway's shared interaction
/// lane while it waits. Past this bound a request is refused with
/// [`LfgError::Busy`] instead of queueing, so a burst of sign-ups cannot fill
/// the lane and starve unrelated commands.
pub const LFG_MAX_WAITING: usize = 8;

/// Lifecycle policy: stop signups at start, close after a 15-minute grace,
/// and reconcile unrecorded deliveries after five minutes. A tick never
/// examines more than 20 candidates, even if its caller requests more.
pub const LFG_EXPIRY_GRACE: time::Duration = time::Duration::minutes(15);
pub const LFG_ORPHAN_AGE: time::Duration = time::Duration::minutes(5);
pub const LFG_SWEEP_MAX_ROWS: usize = 20;

/// Store-backed feature service. The shared interaction runtime owns routing and replies.
#[derive(Debug)]
pub struct LfgInteractions {
    pool: sqlx::PgPool,
    in_flight: tokio::sync::Semaphore,
    /// Executions that are running or queued for the permit.
    admitted: std::sync::atomic::AtomicUsize,
    signup_cooldowns: tokio::sync::Mutex<ActorCooldowns>,
}

/// One admitted execution; releases its place in the queue on drop, including
/// when the owning task is aborted.
struct Admission<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl LfgInteractions {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            in_flight: tokio::sync::Semaphore::new(LFG_MAX_IN_FLIGHT),
            admitted: std::sync::atomic::AtomicUsize::new(0),
            signup_cooldowns: tokio::sync::Mutex::new(ActorCooldowns::default()),
        }
    }

    /// Extract the rendered `components` array for a send, degrading to an
    /// empty list when it is missing or not an array. Mirrors the adjacent
    /// `content` degradation (`unwrap_or_default()`): the send proceeds.
    #[must_use]
    pub fn payload_components(payload: &Value) -> &[Value] {
        match payload.get("components").and_then(Value::as_array) {
            Some(components) => components.as_slice(),
            None => {
                tracing::error!(
                    "lfg render degraded: missing or non-array components; sending with empty components"
                );
                &[]
            }
        }
    }

    /// Take a place in the bounded queue, then wait for the execution permit.
    /// Refuses immediately, holding nothing, when the queue is full.
    async fn admit(&self) -> Result<(Admission<'_>, tokio::sync::SemaphorePermit<'_>), LfgError> {
        use std::sync::atomic::Ordering;
        let place = self.admitted.fetch_add(1, Ordering::AcqRel);
        let admission = Admission(&self.admitted);
        if place >= LFG_MAX_IN_FLIGHT + LFG_MAX_WAITING {
            return Err(LfgError::Busy);
        }
        let permit = self
            .in_flight
            .acquire()
            .await
            .expect("LFG semaphore is never closed");
        Ok((admission, permit))
    }

    /// Also used by routing before its deferred callback. Direct service callers
    /// still pass this gate in execute; the same event does not extend a window.
    pub(crate) async fn admit_signup(
        &self,
        request: &LfgRequest,
        guild_id: &str,
        actor_id: &str,
        interaction_id: u64,
    ) -> Result<(), LfgError> {
        if !matches!(request, LfgRequest::Select(LfgSelectAction::Signup { .. })) {
            return Ok(());
        }
        let guild = guild_id.parse::<u64>().ok().filter(|id| *id != 0);
        let actor = actor_id.parse::<u64>().ok().filter(|id| *id != 0);
        let (Some(guild), Some(actor)) = (guild, actor) else {
            return Err(LfgError::Invalid("Invalid LFG actor or server.".into()));
        };
        if !self.signup_cooldowns.lock().await.admit(
            guild,
            actor,
            interaction_id,
            tokio::time::Instant::now(),
        ) {
            return Err(LfgError::CoolingDown);
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        executor: &ActionExecutor,
        request: LfgRequest,
        guild_id: &str,
        actor_id: &str,
        interaction_id: u64,
        bot_user_id: u64,
    ) -> Result<String, LfgError> {
        // Preview answers before identity derivation, queue admission, locks,
        // SQL, audit or channel effects: pure validation plus the creation
        // body renderer, with no store dependency at all.
        if let LfgRequest::Preview {
            title,
            starts_at,
            roles,
        } = &request
        {
            let now = time::OffsetDateTime::now_utc();
            return lfg::preview_lfg_content(
                title,
                starts_at,
                roles,
                (now.unix_timestamp_nanos() / 1_000_000) as i64,
            )
            .map_err(LfgError::Invalid);
        }
        let id = match &request {
            LfgRequest::Create { .. } => format!("lfg-{interaction_id}"),
            // Preview returns above, before any persisted identity exists.
            LfgRequest::Preview { .. } => {
                return Err(LfgError::Invalid("Invalid LFG preview.".into()));
            }
            LfgRequest::Close { post_id }
            | LfgRequest::Edit { post_id, .. }
            | LfgRequest::Select(
                LfgSelectAction::Signup { post_id, .. } | LfgSelectAction::Leave { post_id },
            ) => {
                if !lfg::valid_post_id(post_id) {
                    return Err(LfgError::Invalid(
                        "LFG id must be between 1 and 128 characters.".into(),
                    ));
                }
                post_id.clone()
            }
        };
        // Apply only to signup/move, before queue admission, SQL, audit or
        // channel refresh. Leave and close must remain usable for recovery.
        self.admit_signup(&request, guild_id, actor_id, interaction_id)
            .await?;
        // Cap pool use before taking a connection: blocked lock waiters must not
        // starve the lock holder or the gateway checkpoint writer. The queue is
        // bounded too, so waiters cannot fill the shared interaction lane.
        let _admitted = self.admit().await?;
        // Serialize store changes + refresh across runtime instances. This key is
        // distinct from the capacity/close lock taken by the domain store.
        let mut guard = self.pool.begin().await?;
        let select_identity = match &request {
            LfgRequest::Select(select) => Some(select_fingerprint(select, guild_id, actor_id)),
            _ => None,
        };
        if select_identity.is_some() {
            // Serialize the identity as well as the post: divergent deliveries
            // can name different posts or guilds, including across processes.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("lfg-select:{interaction_id}"))
                .execute(&mut *guard)
                .await?;
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("lfg-runtime:{guild_id}:{id}"))
            .execute(&mut *guard)
            .await?;
        // Find the interaction before filtering its request fields; otherwise
        // divergence looks like a new request. Legacy rows without a binding
        // cannot prove identical redelivery and are conservatively refused.
        let replay_outcome: Option<String> = if let Some(identity) = &select_identity {
            let audit_id = format!("lfg:signup:{interaction_id}");
            let outcome: Option<String> =
                sqlx::query_scalar("SELECT outcome FROM announcements_audit_log WHERE id = $1")
                    .bind(&audit_id)
                    .fetch_optional(&self.pool)
                    .await?;
            if outcome.is_some() {
                let bound: Option<String> = sqlx::query_scalar(
                    "SELECT reason FROM announcements_audit_log WHERE id = $1 AND reason = $2",
                )
                .bind(&audit_id)
                .bind(identity)
                .fetch_optional(&self.pool)
                .await?;
                if bound.as_ref() != Some(identity) {
                    return Err(LfgError::Invalid(
                        "LFG interaction does not match its saved request.".into(),
                    ));
                }
            }
            outcome
        } else {
            None
        };
        // Queue/lock waits may cross the start time. Judge a new signup at
        // execution, not at arrival; a saved interaction receipt still wins.
        let now = time::OffsetDateTime::now_utc();
        let at = lfg::iso_millis_utc(now);
        let (action, outcome, reply, refresh) = match request {
            LfgRequest::Create {
                title,
                starts_at,
                roles,
                channel_id,
            } => {
                let title =
                    lfg::validate_title(&title).map_err(|e| LfgError::Invalid(e.to_string()))?;
                let starts_at = lfg::normalize_starts_at(
                    &starts_at,
                    (now.unix_timestamp_nanos() / 1_000_000) as i64,
                )
                .map_err(|e| LfgError::Invalid(e.to_string()))?;
                let roles = lfg::spec_roles(
                    &id,
                    &lfg::parse_role_spec(&roles).map_err(|e| LfgError::Invalid(e.to_string()))?,
                );
                let existing = store::get_lfg(&self.pool, guild_id, &id).await?;
                let fresh = existing.is_none();
                let post = existing.unwrap_or(LfgPost {
                    id: id.clone(),
                    guild_id: guild_id.into(),
                    channel_id,
                    message_id: None,
                    title,
                    starts_at,
                    status: LfgStatus::Open,
                    created_by: actor_id.into(),
                    created_at: at.clone(),
                    closed_at: None,
                });
                // A closed row is a replay fence, never permission to resend.
                // Still recover an accepted message so a closed post does not
                // retain stale signup controls after an uncertain delivery.
                if post.status != LfgStatus::Open {
                    if post.message_id.is_none() && bot_user_id != 0 {
                        if let Some(message_id) = executor
                            .recover_message_by_nonce(
                                &post.channel_id,
                                &lfg::lfg_nonce(&id),
                                bot_user_id,
                            )
                            .await
                            .map_err(|_| LfgError::Uncertain)?
                        {
                            store::save_lfg_message_id(&self.pool, guild_id, &id, &message_id)
                                .await?;
                        }
                    }
                    let reply = "This LFG is already closed; create a new post with /lfg.";
                    // A prior recovery may have saved the ID before its edit
                    // failed. Retry that edit too, without changing the result.
                    let reply = match self.refresh(executor, guild_id, &id).await {
                        Ok(()) => reply.into(),
                        Err(_) => format!(
                            "{reply} Message refresh failed; the saved result above still applies."
                        ),
                    };
                    guard.commit().await?;
                    return Ok(reply);
                }
                if fresh {
                    store::put_lfg(&self.pool, &post, &roles, true).await?;
                }
                if post.message_id.is_none() {
                    let nonce = lfg::lfg_nonce(&id);
                    let recovered = if fresh {
                        None
                    } else {
                        executor
                            .recover_message_by_nonce(&post.channel_id, &nonce, bot_user_id)
                            .await
                            .map_err(|_| LfgError::Uncertain)?
                    };
                    let accepted = match recovered {
                        Some(id) => id,
                        None => {
                            let roles = store::list_lfg_roles(&self.pool, &id).await?;
                            let signups = store::list_lfg_signups(&self.pool, &id).await?;
                            let payload = message_payload(&post, &roles, &signups);
                            let sent = executor
                                .post_message_with_components(
                                    &post.channel_id,
                                    payload["content"].as_str().unwrap_or_default(),
                                    Self::payload_components(&payload),
                                    &nonce,
                                )
                                .await;
                            match sent {
                                Ok(id) if id.parse::<u64>().is_ok_and(|v| v != 0) => id,
                                result => {
                                    let error = result.err().unwrap_or_else(|| {
                                        DiscordError::Unavailable(
                                            "accepted message missing id".into(),
                                        )
                                    });
                                    let recovered = if error.is_safe_pre_mutation() {
                                        None
                                    } else {
                                        executor
                                            .recover_message_by_nonce(
                                                &post.channel_id,
                                                &nonce,
                                                bot_user_id,
                                            )
                                            .await
                                            .map_err(|_| LfgError::Uncertain)?
                                    };
                                    if let Some(id) = recovered {
                                        id
                                    } else {
                                        // Both definitive no-send paths share cleanup;
                                        // unreadable recovery still retains the row.
                                        store::delete_lfg(&self.pool, guild_id, &id).await?;
                                        self.audit(
                                            guild_id,
                                            actor_id,
                                            interaction_id,
                                            &id,
                                            "lfg.create",
                                            "failed",
                                            &at,
                                        )
                                        .await?;
                                        return Err(error.into());
                                    }
                                }
                            }
                        }
                    };
                    store::save_lfg_message_id(&self.pool, guild_id, &id, &accepted).await?;
                }
                (
                    "lfg.create",
                    "posted".to_owned(),
                    lfg::created_reply(&id),
                    !fresh,
                )
            }
            LfgRequest::Edit {
                title, starts_at, ..
            } => {
                if title.is_none() && starts_at.is_none() {
                    return Err(LfgError::Invalid(
                        "Provide title or starts-at to edit.".into(),
                    ));
                }
                let title = title
                    .as_deref()
                    .map(lfg::validate_title)
                    .transpose()
                    .map_err(|error| LfgError::Invalid(error.to_string()))?;
                // Use the clock after waiting for the post lock, not admission.
                let starts_at = starts_at
                    .as_deref()
                    .map(|value| {
                        lfg::normalize_starts_at(
                            value,
                            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                                as i64,
                        )
                    })
                    .transpose()
                    .map_err(|error| LfgError::Invalid(error.to_string()))?;
                let starts_at = match store::edit_lfg_metadata(
                    &self.pool,
                    guild_id,
                    &id,
                    title.as_deref(),
                    starts_at.as_deref(),
                )
                .await?
                {
                    store::LfgEditOutcome::Missing => {
                        return Err(LfgError::Invalid(
                            "LFG post is missing in this server.".into(),
                        ));
                    }
                    store::LfgEditOutcome::Closed => {
                        return Err(LfgError::Invalid(
                            "LFG post is closed and cannot be edited.".into(),
                        ));
                    }
                    store::LfgEditOutcome::Unchanged { starts_at } => {
                        guard.commit().await?;
                        return Ok(format!("LFG unchanged. Starts at {starts_at}."));
                    }
                    store::LfgEditOutcome::Updated { starts_at } => starts_at,
                };
                (
                    "lfg.edit",
                    "updated".into(),
                    format!("LFG edit saved. Starts at {starts_at}."),
                    true,
                )
            }
            // Preview returns before admission; reaching the store would be a
            // routing defect, so fail closed without touching it.
            LfgRequest::Preview { .. } => {
                return Err(LfgError::Invalid("Invalid LFG preview.".into()));
            }
            LfgRequest::Close { .. } => {
                let closed = store::close_lfg(&self.pool, guild_id, &id, &at).await?;
                (
                    "lfg.close",
                    if closed {
                        "closed"
                    } else {
                        "already_closed_or_missing"
                    }
                    .into(),
                    lfg::close_reply(closed),
                    true,
                )
            }
            LfgRequest::Select(LfgSelectAction::Signup { role_key, .. }) => {
                let outcome = match replay_outcome.as_ref() {
                    Some(outcome) => outcome.clone(),
                    None => {
                        let post = store::get_lfg(&self.pool, guild_id, &id).await?;
                        if post.as_ref().is_some_and(|post| {
                            time::OffsetDateTime::parse(
                                &post.starts_at,
                                &time::format_description::well_known::Rfc3339,
                            )
                            .is_ok_and(|starts_at| starts_at <= now)
                        }) {
                            "started".to_owned()
                        } else {
                            store::signup_lfg(&self.pool, guild_id, &id, &role_key, actor_id, &at)
                                .await?
                                .as_str()
                                .to_owned()
                        }
                    }
                };
                let reply = if outcome == "started" {
                    "This LFG has already started; ask an organizer to create a new post with /lfg."
                        .to_owned()
                } else {
                    format!("LFG {outcome}.")
                };
                ("lfg.signup", outcome, reply, true)
            }
            LfgRequest::Select(LfgSelectAction::Leave { .. }) => {
                let outcome = match replay_outcome.as_ref() {
                    Some(outcome) => outcome.clone(),
                    None => {
                        // leave_lfg is id-only: establish the guild fence first.
                        let exists = store::get_lfg(&self.pool, guild_id, &id).await?.is_some();
                        let removed = exists && store::leave_lfg(&self.pool, &id, actor_id).await?;
                        if removed { "left" } else { "not_signed_up" }.into()
                    }
                };
                let reply = lfg::leave_reply(outcome == "left");
                ("lfg.leave", outcome, reply, true)
            }
        };
        let audited = if replay_outcome.is_some() {
            true
        } else {
            let audit = if let Some(identity) = select_identity {
                // Keep the existing signup ID namespace, shared with leave so
                // changing the action cannot evade the replay fence.
                rsvp_store::write_audit(
                    &self.pool,
                    &RsvpAudit {
                        id: format!("lfg:signup:{interaction_id}"),
                        guild_id: guild_id.into(),
                        actor_id: Some(actor_id.into()),
                        action: action.into(),
                        target_key: Some(id.clone()),
                        outcome: outcome.clone(),
                        reason: Some(identity),
                        created_at: at.clone(),
                    },
                )
                .await
                .map(|_| ())
                .map_err(LfgError::from)
            } else {
                self.audit(
                    guild_id,
                    actor_id,
                    interaction_id,
                    &id,
                    action,
                    &outcome,
                    &at,
                )
                .await
            };
            match audit {
                Ok(()) => true,
                // Create/nonce recovery has its own failure contract.
                Err(error) if action == "lfg.create" => return Err(error),
                Err(_) => false,
            }
        };
        let result = if !audited {
            // Store calls committed independently of this lock-only transaction.
            // Do not turn applied state into a bare failure or claim an audit.
            if action == "lfg.edit" {
                format!("{reply} Audit could not be recorded; the visible post could not be refreshed. Check the saved state before retrying.")
            } else {
                format!(
                    "{reply} Audit could not be recorded; check the saved state before retrying."
                )
            }
        } else if refresh {
            match self.refresh(executor, guild_id, &id).await {
                Ok(()) => reply,
                Err(_) if action == "lfg.edit" => {
                    format!("{reply} The visible post could not be refreshed.")
                }
                Err(_) => {
                    format!("{reply} Message refresh failed; the saved result above still applies.")
                }
            }
        } else {
            reply
        };
        guard.commit().await?;
        Ok(result)
    }

    /// Process a bounded lifecycle batch for one configured guild. The jobs
    /// supervisor is the only production caller; row-local failures are logged
    /// without aborting later rows. The count includes failed row attempts.
    /// Besides expiring open posts, each tick retries the durable
    /// close-refresh intents that failed edits left behind: closed rows whose
    /// pending flag is armed and whose backoff has elapsed share the same
    /// 20-row cap and per-tick budget as expiry work.
    pub async fn sweep_lifecycle(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        bot_user_id: u64,
        now: time::OffsetDateTime,
        limit: usize,
    ) -> Result<usize, LfgError> {
        let limit = limit.min(LFG_SWEEP_MAX_ROWS);
        if limit == 0 {
            return Ok(0);
        }
        // Never treat an unresolved identity as evidence that a nonce is absent.
        if bot_user_id == 0 {
            return Err(LfgError::Uncertain);
        }
        let expired_before = now.saturating_sub(LFG_EXPIRY_GRACE);
        let orphan_before = now.saturating_sub(LFG_ORPHAN_AGE);
        let candidates: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM lfg_posts WHERE guild_id = $1
             AND ((status = 'open'
                   AND (starts_at <= $2 OR (message_id IS NULL AND created_at <= $3)))
                  OR (status = 'closed' AND close_refresh_pending
                      AND (close_refresh_next_at IS NULL OR close_refresh_next_at <= $4)))
             ORDER BY created_at, id LIMIT $5",
        )
        .bind(guild_id)
        .bind(expired_before)
        .bind(orphan_before)
        .bind(now)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut processed = 0;
        for (id,) in candidates {
            match self
                .sweep_lifecycle_row(executor, guild_id, bot_user_id, now, &id)
                .await
            {
                Ok(true) => processed += 1,
                Ok(false) => {}
                Err(_) => {
                    processed += 1;
                    // Never log raw SQL/REST errors or member-controlled text.
                    lifecycle_warning("lfg lifecycle row failed; continuing bounded batch");
                }
            }
        }
        tracing::info!(processed, "lfg lifecycle batch completed");
        Ok(processed)
    }

    async fn sweep_lifecycle_row(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        bot_user_id: u64,
        now: time::OffsetDateTime,
        id: &str,
    ) -> Result<bool, LfgError> {
        let _admitted = self.admit().await?;
        let mut guard = self.pool.begin().await?;
        // Same lock as interaction refresh. A contended row is left for the
        // next tick rather than holding the batch behind a live interaction.
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("lfg-runtime:{guild_id}:{id}"))
                .fetch_one(&mut *guard)
                .await?;
        if !locked {
            return Ok(false);
        }
        let Some(post) = store::get_lfg(&self.pool, guild_id, id).await? else {
            return Ok(false);
        };
        if post.status == LfgStatus::Closed {
            return self.repair_closed_row(executor, guild_id, &post, now).await;
        }
        let parse = |at: &str| {
            time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                .map_err(|_| LfgError::Invalid("Invalid saved LFG timestamp.".into()))
        };
        let expired = parse(&post.starts_at)? <= now.saturating_sub(LFG_EXPIRY_GRACE);
        let orphan = post.message_id.is_none()
            && parse(&post.created_at)? <= now.saturating_sub(LFG_ORPHAN_AGE);
        if !expired && !orphan {
            return Ok(false);
        }
        let mut unrecovered = false;
        if post.message_id.is_none() {
            match executor
                .recover_message_by_nonce(&post.channel_id, &lfg::lfg_nonce(id), bot_user_id)
                .await
            {
                Ok(Some(message_id)) => {
                    store::save_lfg_message_id(&self.pool, guild_id, id, &message_id).await?;
                }
                Ok(None) => unrecovered = true,
                Err(_) => {
                    // Keep the durable row, but release its open-post capacity.
                    // An unreadable channel is never permission to resend.
                    unrecovered = true;
                    lifecycle_warning("lfg orphan recovery unreadable; closing retained row");
                }
            }
        }
        if expired || unrecovered {
            store::close_lfg(&self.pool, guild_id, id, &lfg::iso_millis_utc(now)).await?;
        }
        // A recovered expired post must also lose its signup controls.
        self.refresh(executor, guild_id, id).await?;
        guard.commit().await?;
        Ok(true)
    }

    /// Retry one durable close-message edit under the runtime lock already
    /// held by [`Self::sweep_lifecycle_row`]. Repairs only the exact saved
    /// message of a still-pending closed post: a confirmed edit or a
    /// definitive Unknown Message outcome settles the intent, while any other
    /// failure defers it with durable backoff. Never recreates a message,
    /// sends, deletes or reopens. Returns `true` when an attempt ran.
    async fn repair_closed_row(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        post: &LfgPost,
        now: time::OffsetDateTime,
    ) -> Result<bool, LfgError> {
        // The candidate scan already filtered to pending, due rows; re-check
        // under the held runtime lock so a concurrent repair cannot double-PATCH.
        let state = store::get_lfg_close_refresh(&self.pool, guild_id, &post.id).await?;
        let eligible =
            state.is_some_and(|state| state.pending && state.next_at.is_none_or(|due| due <= now));
        if !eligible {
            return Ok(false);
        }
        let attempts = state.map(|state| state.attempts).unwrap_or(0);
        let Some(message_id) = post.message_id.as_deref() else {
            // No saved message to repair: settle quietly without wire work.
            // Uncertainty never authorizes a replacement send.
            store::clear_lfg_close_refresh(&self.pool, guild_id, &post.id).await?;
            return Ok(false);
        };
        let roles = store::list_lfg_roles(&self.pool, &post.id).await?;
        let signups = store::list_lfg_signups(&self.pool, &post.id).await?;
        // Rebuilt from current saved state, never a stale open snapshot: a
        // closed post always renders with its controls removed.
        let payload = message_payload(post, &roles, &signups);
        match executor
            .edit_message_for_close_refresh(
                &post.channel_id,
                message_id,
                payload["content"].as_str().unwrap_or_default(),
                Self::payload_components(&payload),
            )
            .await
        {
            CloseRefreshEdit::Edited => {
                store::clear_lfg_close_refresh(&self.pool, guild_id, &post.id).await?;
                tracing::info!(
                    outcome = "repaired",
                    post_id = %post.id,
                    "lfg close repair removed stale controls"
                );
                Ok(true)
            }
            CloseRefreshEdit::UnknownMessage => {
                store::clear_lfg_close_refresh(&self.pool, guild_id, &post.id).await?;
                tracing::info!(
                    outcome = "settled_missing",
                    error_class = "unknown_message",
                    post_id = %post.id,
                    "lfg close repair settled a deleted message without recreating it"
                );
                Ok(true)
            }
            CloseRefreshEdit::Retryable(error) => {
                let attempts = attempts.saturating_add(1);
                store::defer_lfg_close_refresh(
                    &self.pool,
                    guild_id,
                    &post.id,
                    attempts,
                    now.saturating_add(close_refresh_backoff(attempts)),
                )
                .await?;
                // Fixed-vocabulary classification only: never the error text
                // (it can carry transport detail), member text or SQL.
                tracing::warn!(
                    outcome = "retry_scheduled",
                    error_class = error.cause(),
                    post_id = %post.id,
                    "lfg close repair deferred; retry stays bounded"
                );
                Ok(true)
            }
        }
    }

    async fn refresh(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        id: &str,
    ) -> Result<(), LfgError> {
        let Some(post) = store::get_lfg(&self.pool, guild_id, id).await? else {
            return Ok(());
        };
        let Some(message_id) = post.message_id.as_deref() else {
            // A closed post with no saved message has nothing to repair.
            if post.status == LfgStatus::Closed {
                store::clear_lfg_close_refresh(&self.pool, guild_id, id).await?;
            }
            return Ok(());
        };
        let roles = store::list_lfg_roles(&self.pool, id).await?;
        let signups = store::list_lfg_signups(&self.pool, id).await?;
        let payload = message_payload(&post, &roles, &signups);
        executor
            .edit_message_with_components(
                &post.channel_id,
                message_id,
                payload["content"].as_str().unwrap_or_default(),
                Self::payload_components(&payload),
            )
            .await?;
        // A confirmed edit of a closed message satisfies any pending
        // close-refresh intent armed by its close transition.
        if post.status == LfgStatus::Closed {
            store::clear_lfg_close_refresh(&self.pool, guild_id, id).await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        guild: &str,
        actor: &str,
        interaction: u64,
        target: &str,
        action: &str,
        outcome: &str,
        at: &str,
    ) -> Result<(), LfgError> {
        rsvp_store::write_audit(
            &self.pool,
            &RsvpAudit {
                id: if action == "lfg.signup" {
                    format!("lfg:signup:{interaction}")
                } else {
                    format!(
                        "lfg:{interaction}:{}",
                        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                    )
                },
                guild_id: guild.into(),
                actor_id: Some(actor.into()),
                action: action.into(),
                target_key: Some(target.into()),
                outcome: outcome.into(),
                reason: None,
                created_at: at.into(),
            },
        )
        .await?;
        Ok(())
    }
}

// One callsite for both constant warnings keeps tracing metadata shared.
fn lifecycle_warning(message: &'static str) {
    tracing::warn!(message = message);
}

/// Delay before the next close-refresh attempt after `attempts` recorded
/// failures: 5 minutes doubling per attempt, capped at 6 hours. Same-tick
/// restarts never hammer Discord; a day-separated tick is always eligible.
fn close_refresh_backoff(attempts: i32) -> time::Duration {
    let shift = (attempts.max(0) as u32).min(7);
    let minutes = 5_i64.saturating_mul(1_i64 << shift);
    time::Duration::minutes(minutes).min(time::Duration::hours(6))
}

/// Versioned, length-delimited request binding. Hashing keeps the audit reason
/// bounded and avoids copying member-controlled role text into the audit trail.
fn select_fingerprint(select: &LfgSelectAction, guild: &str, actor: &str) -> String {
    let (post, action, role) = match select {
        LfgSelectAction::Signup { post_id, role_key } => {
            (post_id.as_str(), "signup", role_key.as_str())
        }
        LfgSelectAction::Leave { post_id } => (post_id.as_str(), "leave", ""),
    };
    let mut hash = Sha256::new();
    for field in ["lfg-select:v1", guild, actor, post, action, role] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("lfg-select:v1:{}", hex::encode(hash.finalize()))
}

/// Legacy message shape. Closed posts remove the select; user references never ping.
/// Component contract: <https://docs.discord.com/developers/components/reference#string-select>
pub fn message_payload(post: &LfgPost, roles: &[LfgRole], signups: &[LfgSignup]) -> Value {
    let options = lfg::lfg_select_options(post.status, roles, signups);
    let components = if options.is_empty() {
        json!([])
    } else {
        json!([{
            "type": 1,
            "components": [{
                "type": 3,
                "custom_id": lfg::lfg_custom_id(&post.id),
                "placeholder": "Pick a role or leave",
                "min_values": 1,
                "max_values": 1,
                "options": options.into_iter().map(|option| json!({
                    "label": option.label, "value": option.value
                })).collect::<Vec<_>>()
            }]
        }])
    };
    json!({
        "content": lfg::lfg_content(post, roles, signups),
        "components": components,
        "allowed_mentions": { "parse": [] }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::Ordering, Arc};

    fn service() -> Arc<LfgInteractions> {
        // Never connects: admission happens before any pool use.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        Arc::new(LfgInteractions::new(pool))
    }

    /// Whether a request got through; its place and permit drop on return.
    async fn admitted(service: Arc<LfgInteractions>) -> bool {
        let result = service.admit().await;
        result.is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn signup_refusal_precedes_queue_and_pool_and_exempts_recovery() {
        let service = service();
        service.pool.close().await;
        let executor = ActionExecutor::with_proxy(
            "lfg-admission-fixture".into(),
            Some("http://127.0.0.1:1".into()),
        )
        .unwrap();
        let request = || {
            LfgRequest::Select(LfgSelectAction::Signup {
                post_id: "post".into(),
                role_key: "tank".into(),
            })
        };
        service.admit_signup(&request(), "1", "2", 3).await.unwrap();
        let running = service.admit().await.unwrap();
        for id in 4..1004 {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                service.execute(&executor, request(), "1", "2", id, 0),
            )
            .await
            .expect("refusal cannot queue or access the closed pool");
            assert!(matches!(result, Err(LfgError::CoolingDown)));
        }
        assert_eq!(service.admitted.load(Ordering::Acquire), 1);
        assert!(service.admit_signup(&request(), "1", "2", 3).await.is_ok());
        assert!(service.admit_signup(&request(), "1", "4", 5).await.is_ok());
        assert!(service.admit_signup(&request(), "6", "2", 5).await.is_ok());
        for request in [
            LfgRequest::Select(LfgSelectAction::Leave {
                post_id: "post".into(),
            }),
            LfgRequest::Close {
                post_id: "post".into(),
            },
        ] {
            assert!(service.admit_signup(&request, "1", "2", 4).await.is_ok());
        }
        tokio::time::advance(crate::automation_admission::COOLDOWN).await;
        assert!(service.admit_signup(&request(), "1", "2", 4).await.is_ok());
        drop(running);
    }

    #[tokio::test]
    async fn queue_is_bounded_and_refuses_instead_of_waiting() {
        let service = service();
        let running = service.admit().await.expect("first request runs");
        let waiting: Vec<_> = (0..LFG_MAX_WAITING)
            .map(|_| {
                let service = Arc::clone(&service);
                tokio::spawn(admitted(service))
            })
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while service.admitted.load(Ordering::Acquire) < LFG_MAX_IN_FLIGHT + LFG_MAX_WAITING {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiters queue behind the running request");

        let refused = tokio::time::timeout(std::time::Duration::from_secs(1), service.admit())
            .await
            .expect("a full queue refuses immediately");
        assert!(matches!(refused, Err(LfgError::Busy)));
        let Err(error) = refused else {
            unreachable!("a full queue refuses instead of waiting");
        };
        assert_eq!(
            error.to_string(),
            "LFG is busy right now; try again in a few seconds."
        );
        assert_eq!(
            service.admitted.load(Ordering::Acquire),
            LFG_MAX_IN_FLIGHT + LFG_MAX_WAITING,
            "a refused request leaves no place held"
        );

        drop(running);
        for waiter in waiting {
            assert!(waiter.await.expect("waiter task"), "queued work proceeds");
        }
        assert_eq!(service.admitted.load(Ordering::Acquire), 0);
        assert!(service.admit().await.is_ok(), "the queue drains fully");
    }

    #[tokio::test]
    async fn aborted_waiter_gives_up_its_place() {
        let service = service();
        let running = service.admit().await.expect("first request runs");
        let waiter = {
            let service = Arc::clone(&service);
            tokio::spawn(admitted(service))
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while service.admitted.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter queues");
        waiter.abort();
        let _ = waiter.await;
        drop(running);
        assert_eq!(
            service.admitted.load(Ordering::Acquire),
            0,
            "gateway shutdown cancels queued work without leaking places"
        );
    }

    #[test]
    fn select_fingerprint_is_versioned_bounded_and_stable() {
        let signup = LfgSelectAction::Signup {
            post_id: "post-a".into(),
            role_key: "tank".into(),
        };
        assert_eq!(
            select_fingerprint(&signup, "1", "2"),
            "lfg-select:v1:60115d1e043296e19120f8c252c61372a98fbb8571534bdbf5453b49393e2e3c"
        );
        let hostile = LfgSelectAction::Signup {
            post_id: "post-a".into(),
            role_key: "@everyone".repeat(1000),
        };
        let fingerprint = select_fingerprint(&hostile, "1", "2");
        assert_eq!(fingerprint.len(), 78);
        assert!(!fingerprint.contains("@everyone"));
        assert_ne!(fingerprint, select_fingerprint(&signup, "1", "2"));
    }

    #[test]
    fn select_fingerprint_distinguishes_field_boundaries_and_leave() {
        let signup = LfgSelectAction::Signup {
            post_id: "post-a".into(),
            role_key: String::new(),
        };
        assert_ne!(
            select_fingerprint(&signup, "1", "23"),
            select_fingerprint(&signup, "12", "3")
        );
        assert_ne!(
            select_fingerprint(&signup, "1", "2"),
            select_fingerprint(
                &LfgSelectAction::Leave {
                    post_id: "post-a".into()
                },
                "1",
                "2"
            )
        );
    }

    #[test]
    fn close_refresh_backoff_suppresses_same_tick_and_allows_next_day() {
        assert_eq!(close_refresh_backoff(0), time::Duration::minutes(5));
        assert_eq!(close_refresh_backoff(1), time::Duration::minutes(10));
        assert_eq!(close_refresh_backoff(-3), time::Duration::minutes(5));
        for attempts in 0..1000 {
            let backoff = close_refresh_backoff(attempts);
            assert!(backoff > time::Duration::ZERO, "same-tick retries wait");
            assert!(
                backoff <= time::Duration::hours(6),
                "a day-later tick is eligible"
            );
        }
        assert_eq!(close_refresh_backoff(1000), time::Duration::hours(6));
    }

    #[test]
    fn busy_and_uncertain_displays_are_pinned() {
        // Busy renders member-visible via `busy.to_string()` (interactions.rs),
        // so this Display pin is the member copy; the admission refusal path
        // pins the same string in
        // `queue_is_bounded_and_refuses_instead_of_waiting`.
        assert_eq!(
            LfgError::Busy.to_string(),
            "LFG is busy right now; try again in a few seconds."
        );
        assert_eq!(
            LfgError::Uncertain.to_string(),
            "LFG acceptance is uncertain; durable state retained for nonce recovery"
        );
        // The member-visible Uncertain reply is a different literal at its
        // render site (interactions.rs), pinned exact by the
        // `router_cleans_failed_posts_but_preserves_unreadable_acceptance`
        // integration test — never this Display.
        assert_ne!(
            LfgError::Uncertain.to_string(),
            "LFG post acceptance is uncertain; saved state retained for nonce recovery."
        );
    }
}
