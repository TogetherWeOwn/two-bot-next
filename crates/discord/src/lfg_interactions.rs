//! LFG interaction handlers using the shared router, store and REST executor.

use crate::{ActionExecutor, DiscordError};
use serde_json::{json, Value};
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
    Close {
        post_id: String,
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
    #[error("LFG acceptance is uncertain; durable state retained for nonce recovery")]
    Uncertain,
}

/// LFG executions allowed to hold a pool connection at once, per process.
///
/// Each execution keeps its advisory-lock transaction open across paced REST
/// and takes one more connection for every store call. With the production
/// pool of 5 (`two_bot_store::pool::DB_POOL_MAX`), one in-flight execution uses
/// at most 2, so the gateway checkpoint writer and the other features always
/// find a free connection. Waiters queue here before `begin()`, holding none.
pub const LFG_MAX_IN_FLIGHT: usize = 1;

/// Store-backed feature service. The shared interaction runtime owns routing and replies.
#[derive(Debug)]
pub struct LfgInteractions {
    pool: sqlx::PgPool,
    in_flight: tokio::sync::Semaphore,
}

impl LfgInteractions {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            in_flight: tokio::sync::Semaphore::new(LFG_MAX_IN_FLIGHT),
        }
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
        let now = time::OffsetDateTime::now_utc();
        let at = lfg::iso_millis_utc(now);
        let id = match &request {
            LfgRequest::Create { .. } => format!("lfg-{interaction_id}"),
            LfgRequest::Close { post_id } => post_id.clone(),
            LfgRequest::Select(
                LfgSelectAction::Signup { post_id, .. } | LfgSelectAction::Leave { post_id },
            ) => post_id.clone(),
        };
        // Cap pool use before taking a connection: blocked lock waiters must not
        // starve the lock holder or the gateway checkpoint writer.
        let _permit = self
            .in_flight
            .acquire()
            .await
            .expect("LFG semaphore is never closed");
        // Serialize store changes + refresh across runtime instances. This key is
        // distinct from the capacity/close lock taken by the domain store.
        let mut guard = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("lfg-runtime:{guild_id}:{id}"))
            .execute(&mut *guard)
            .await?;
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
                                    payload["components"].as_array().expect("rendered array"),
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
                                    if error.is_safe_pre_mutation() {
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
                                    match executor
                                        .recover_message_by_nonce(
                                            &post.channel_id,
                                            &nonce,
                                            bot_user_id,
                                        )
                                        .await
                                    {
                                        Ok(Some(id)) => id,
                                        Ok(None) => {
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
                                        Err(_) => return Err(LfgError::Uncertain),
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
                let outcome =
                    store::signup_lfg(&self.pool, guild_id, &id, &role_key, actor_id, &at).await?;
                (
                    "lfg.signup",
                    outcome.as_str().into(),
                    lfg::signup_reply(outcome),
                    true,
                )
            }
            LfgRequest::Select(LfgSelectAction::Leave { .. }) => {
                // leave_lfg is id-only: establish the guild fence before invoking it.
                let exists = store::get_lfg(&self.pool, guild_id, &id).await?.is_some();
                let removed = exists && store::leave_lfg(&self.pool, &id, actor_id).await?;
                (
                    "lfg.leave",
                    if removed { "left" } else { "not_signed_up" }.into(),
                    lfg::leave_reply(removed),
                    true,
                )
            }
        };
        self.audit(
            guild_id,
            actor_id,
            interaction_id,
            &id,
            action,
            &outcome,
            &at,
        )
        .await?;
        let result = if refresh {
            match self.refresh(executor, guild_id, &id).await {
                Ok(()) => reply,
                Err(_) => format!("{reply} Message refresh failed; the saved state is unchanged."),
            }
        } else {
            reply
        };
        guard.commit().await?;
        Ok(result)
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
                payload["components"].as_array().expect("rendered array"),
            )
            .await?;
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
                id: format!(
                    "lfg:{interaction}:{}",
                    time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                ),
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
