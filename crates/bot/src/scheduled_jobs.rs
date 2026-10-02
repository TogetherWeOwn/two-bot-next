//! The 15 s scheduled-message ticker (legacy `runDueScheduled` in
//! `src/automations/service.ts`, driven by `scheduler.ts`).
//!
//! Each tick claims at most [`TICKER_BATCH_LIMIT`] due rows, one per
//! `claim_due` call with a fresh claim token. The claim parks `next_run_at`
//! at the 60 s lease horizon, so a second ticker (or a restarted process)
//! sees the row as not due until the lease expires. The post carries the
//! row's stable occurrence nonce with `enforce_nonce`, so a post retried after
//! an ambiguous failure or an expired lease cannot create a second message.
//! The supervisor never overlaps attempts of one job, which replaces the
//! legacy re-entrancy guard.
//!
//! Every claimed occurrence ends in one `scheduled.run` audit row (ids and
//! outcomes only, never the body). The one exception is legacy's too: when a
//! re-queue finds the claim already gone, the new claim holder owns the row.

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use two_bot_core::{
    audit_scheduled, claim_due, clamp_retry_delay_ms, complete_run, format_iso_ms, lease_until_ms,
    post_failure_retryable, retry_scheduled, OccurrenceOutcome, ScheduledAuditInput,
    ScheduledMessageRow, ScheduledStoreError, SCHEDULER_TICK_MS, TICKER_BATCH_LIMIT,
};
use two_bot_discord::executor::{ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordError};

use crate::{
    jobs::{self, ErrorClass, Job, JobAction},
    website_jobs::Context,
};

pub const NAMES: [&str; 1] = ["scheduled_messages"];

const AUDIT_ACTION: &str = "scheduled.run";
const ORPHAN_REASON: &str = "Scheduled message run was not recorded";

#[cfg(test)]
#[path = "scheduled_jobs_tests.rs"]
mod tests;

/// The supervised ticker for the configured guild.
pub(crate) fn register(context: Arc<Context>) -> Job {
    job(Arc::new(move || {
        let context = context.clone();
        Box::pin(
            async move { tick(context.pool().await?, &context.rest, &context.guild, now_ms).await },
        )
    }))
}

/// Ten occurrences at the 5 s REST timeout, each with a possible orphan
/// delete, stay well inside the attempt timeout.
fn job(action: JobAction) -> Job {
    let cadence = Duration::from_millis(SCHEDULER_TICK_MS);
    Job {
        name: NAMES[0],
        cadence,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: Duration::from_secs(120),
        action,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Claim tokens and audit ids: 16 CSPRNG bytes, hex-encoded.
fn random_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// A fresh occurrence nonce: 24 hex chars (legacy `messageNonce`), inside
/// Discord's 25-char string-nonce ceiling.
fn random_nonce() -> String {
    hex::encode(rand::random::<[u8; 12]>())
}

/// What the tick does after one occurrence.
enum Step {
    Next,
    /// Recording the run failed; legacy stops the batch there.
    Stop,
}

/// One supervised attempt with the clock injected. Database failures fail
/// the attempt so the supervisor metrics report them; a failed audit write
/// is logged, finishes the batch, and then fails the attempt too.
pub(crate) async fn tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    clock: impl Fn() -> u64 + Send + Sync,
) -> Result<(), ErrorClass> {
    let mut audit_failed = false;
    for _ in 0..TICKER_BATCH_LIMIT {
        let now = clock();
        let now_iso = format_iso_ms(now);
        let claim_token = random_id();
        let claimed = claim_due(
            pool,
            guild,
            &now_iso,
            &claim_token,
            &format_iso_ms(lease_until_ms(now)),
            &random_nonce(),
        )
        .await
        .map_err(database)?;
        let Some(row) = claimed.into_iter().next() else {
            break;
        };
        let occurrence = Occurrence {
            pool,
            rest,
            row: &row,
            claim_token: &claim_token,
            now,
            now_iso: &now_iso,
        };
        match occurrence.run(&mut audit_failed).await? {
            Step::Next => {}
            Step::Stop => return Err(ErrorClass::Database),
        }
    }
    if audit_failed {
        Err(ErrorClass::Database)
    } else {
        Ok(())
    }
}

struct Occurrence<'a> {
    pool: &'a PgPool,
    rest: &'a ActionExecutor,
    row: &'a ScheduledMessageRow,
    claim_token: &'a str,
    now: u64,
    now_iso: &'a str,
}

impl Occurrence<'_> {
    async fn run(&self, audit_failed: &mut bool) -> Result<Step, ErrorClass> {
        let call = ChannelCall::PostMessage {
            channel_id: self.row.channel_id.clone(),
            content: self.row.body.clone(),
            nonce: self.row.occurrence_nonce.clone(),
        };
        let posted = match self.rest.execute_channel(&call).await {
            Ok(ChannelCallOutcome::Posted { message_id }) => {
                Ok(Some(message_id).filter(|id| !id.is_empty()))
            }
            Ok(_) => Ok(None),
            Err(error) => Err(error),
        };
        match posted {
            Ok(message_id) => self.record_post(message_id, audit_failed).await,
            Err(error) => self.record_failure(&error, audit_failed).await,
        }
    }

    async fn record_post(
        &self,
        message_id: Option<String>,
        audit_failed: &mut bool,
    ) -> Result<Step, ErrorClass> {
        let completed = complete_run(
            self.pool,
            &self.row.guild_id,
            &self.row.id,
            self.now_iso,
            message_id.as_deref(),
            self.claim_token,
        )
        .await;
        match completed {
            Ok(Some(_)) => {
                self.audit(
                    &OccurrenceOutcome::Posted { message_id },
                    None,
                    audit_failed,
                )
                .await;
                Ok(Step::Next)
            }
            Ok(None) => {
                // The definition changed or was cancelled while Discord was
                // posting: remove the orphan rather than let it survive.
                if let Some(message_id) = &message_id {
                    let _ = self.delete(message_id).await;
                }
                self.audit(&OccurrenceOutcome::StaleCompletion, None, audit_failed)
                    .await;
                Ok(Step::Next)
            }
            Err(error) => {
                let reason = store_error_name(&error);
                tracing::warn!(guild = %self.row.guild_id, schedule = %self.row.id, reason, "scheduled run not recorded");
                let cleaned = match &message_id {
                    Some(message_id) => self.delete(message_id).await.is_ok(),
                    None => false,
                };
                // Keep the nonce when the message survives so the re-run
                // dedupes against it instead of posting a second copy.
                let retained = retry_scheduled(
                    self.pool,
                    &self.row.guild_id,
                    &self.row.id,
                    self.claim_token,
                    self.now_iso,
                    !cleaned,
                )
                .await
                .map_err(database)?;
                if retained {
                    self.audit(
                        &OccurrenceOutcome::PostedUnrecorded { cleaned },
                        Some(reason),
                        audit_failed,
                    )
                    .await;
                }
                Ok(Step::Stop)
            }
        }
    }

    async fn record_failure(
        &self,
        error: &DiscordError,
        audit_failed: &mut bool,
    ) -> Result<Step, ErrorClass> {
        let reason = discord_error_name(error);
        if post_failure_retryable(failure_status(error)) {
            let retry_at_ms = self.now.saturating_add(clamp_retry_delay_ms(None));
            let retained = retry_scheduled(
                self.pool,
                &self.row.guild_id,
                &self.row.id,
                self.claim_token,
                &format_iso_ms(retry_at_ms),
                true,
            )
            .await
            .map_err(database)?;
            if retained {
                self.audit(
                    &OccurrenceOutcome::Retryable { retry_at_ms },
                    Some(reason),
                    audit_failed,
                )
                .await;
            }
            return Ok(Step::Next);
        }
        // A permanent refusal runs the occurrence without a message so a
        // dead row cannot wedge the queue: one-shots disable, recurring
        // rows advance.
        let completed = complete_run(
            self.pool,
            &self.row.guild_id,
            &self.row.id,
            self.now_iso,
            None,
            self.claim_token,
        )
        .await
        .map_err(|error| {
            tracing::warn!(guild = %self.row.guild_id, schedule = %self.row.id, reason = store_error_name(&error), "scheduled failure not recorded");
            ErrorClass::Database
        })?;
        if completed.is_some() {
            self.audit(
                &OccurrenceOutcome::FailedPermanent,
                Some(reason),
                audit_failed,
            )
            .await;
        }
        Ok(Step::Next)
    }

    async fn delete(&self, message_id: &str) -> Result<(), DiscordError> {
        self.rest
            .delete_message(&self.row.channel_id, message_id, ORPHAN_REASON)
            .await
    }

    async fn audit(
        &self,
        outcome: &OccurrenceOutcome,
        reason: Option<&str>,
        audit_failed: &mut bool,
    ) {
        let input = ScheduledAuditInput {
            guild_id: self.row.guild_id.clone(),
            actor_id: None,
            action: AUDIT_ACTION.to_owned(),
            target_key: Some(self.row.id.clone()),
            outcome: outcome.audit_outcome().to_owned(),
            reason: reason.map(str::to_owned),
        };
        if let Err(error) = audit_scheduled(self.pool, &input, self.now_iso, &random_id()).await {
            *audit_failed = true;
            tracing::warn!(
                guild = %self.row.guild_id,
                schedule = %self.row.id,
                outcome = outcome.audit_outcome(),
                error = %error,
                "scheduled audit write failed"
            );
        }
    }
}

fn database(error: sqlx::Error) -> ErrorClass {
    tracing::warn!(error = %error, "scheduled message store failed");
    ErrorClass::Database
}

/// The HTTP status legacy `DiscordPostError` would carry: refusals are 4xx,
/// a spent rate limit is 429, and a timeout or unusable answer had none.
fn failure_status(error: &DiscordError) -> Option<u16> {
    match error {
        DiscordError::Rejected(_) => Some(400),
        DiscordError::RateLimited => Some(429),
        DiscordError::Timeout | DiscordError::Unavailable(_) => None,
    }
}

/// Fixed audit reasons (legacy `safeErrorName`): never Discord's text.
fn discord_error_name(error: &DiscordError) -> &'static str {
    match error {
        DiscordError::Rejected(_) => "discord_rejected",
        DiscordError::RateLimited => "discord_rate_limited",
        DiscordError::Timeout => "discord_timeout",
        DiscordError::Unavailable(_) => "discord_unavailable",
    }
}

fn store_error_name(error: &ScheduledStoreError) -> &'static str {
    match error {
        ScheduledStoreError::CorruptNextRunAt { .. } => "corrupt_next_run_at",
        ScheduledStoreError::Sql(_) => "database",
    }
}
