//! The 15 s scheduled-message ticker (legacy `runDueScheduled` in
//! `src/automations/service.ts`, driven by `scheduler.ts`).
//!
//! Each tick claims at most [`TICKER_BATCH_LIMIT`] due rows, one per
//! `claim_due` call with a fresh claim token. The claim parks `next_run_at`
//! at the 60 s lease horizon, so a second ticker (or a restarted process)
//! sees the row as not due until the lease expires. Before POST, a durable
//! uncertainty fence is armed under the claim token. Reclaimed fenced rows
//! never POST or advance: Discord's short nonce window is not replay safety.
//! Only proven no effect or confirmed cleanup permits a new attempt. The
//! supervisor never overlaps attempts of one job.
//!
//! Every claimed occurrence attempts one `scheduled.run` audit write (ids and
//! outcomes only, never the body), including when recording or re-queuing the
//! occurrence fails. An audit-write failure is logged and fails the tick.

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use two_bot_core::{
    activation::LiveCapability, audit_scheduled, begin_scheduled_delivery, claim_due,
    clamp_retry_delay_ms, complete_run, format_iso_ms, lease_until_ms, neutralize_scheduled_body,
    post_failure_retryable, retry_scheduled, FeatureGates, OccurrenceOutcome, ScheduledAuditInput,
    ScheduledMessageRow, ScheduledStoreError, SCHEDULER_TICK_MS, TICKER_BATCH_LIMIT,
};
use two_bot_discord::executor::{ActionExecutor, DiscordError};

use crate::{
    activation::BootActivation,
    db_log::db_class,
    jobs::{self, ErrorClass, Job, JobAction},
    website_jobs::Context,
};

pub const NAMES: [&str; 1] = ["scheduled_messages"];

const AUDIT_ACTION: &str = "scheduled.run";
const ORPHAN_REASON: &str = "Scheduled message run was not recorded";

#[cfg(test)]
#[path = "scheduled_jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "scheduled_stale_audit_acceptance_tests.rs"]
mod acceptance_tests;

#[cfg(test)]
#[path = "scheduled_store_failure_audit_acceptance_tests.rs"]
mod store_failure_acceptance_tests;

#[cfg(test)]
#[path = "scheduled_confirmed_cleanup_tests.rs"]
mod confirmed_cleanup_tests;

/// The supervised ticker for the configured guild, or `None` while
/// `TWO_AUTOMATIONS` is off (legacy only started the scheduler when automations
/// were enabled, and a disabled scheduler fired zero jobs) or the boot
/// identity does not permit automations (the live-identity fence, TOG-15758).
/// A disabled, refused or misconfigured gate builds no job, so no row is
/// claimed, posted or audited.
pub(crate) fn register(context: Arc<Context>, activation: &BootActivation) -> Option<Job> {
    let gates = match FeatureGates::from_env() {
        Ok(gates) => gates,
        Err(_) => {
            tracing::warn!(
                job = NAMES[0],
                "scheduled messages parked: invalid feature configuration"
            );
            return None;
        }
    };
    register_fenced(
        gates,
        activation,
        Arc::new(move || {
            let context = context.clone();
            Box::pin(async move {
                tick(context.pool().await?, &context.rest, &context.guild, now_ms).await
            })
        }),
    )
}

/// Identity can only narrow the env gate: the ticker posts under the token's
/// identity, exactly like the `/schedule` verbs the router refuses there.
pub(crate) fn register_fenced(
    gates: FeatureGates,
    activation: &BootActivation,
    action: JobAction,
) -> Option<Job> {
    let fenced = activation.constrain_features(gates);
    if gates.automations && !fenced.automations {
        tracing::warn!(
            job = NAMES[0],
            capability = LiveCapability::Automations.as_str(),
            "scheduled messages parked: live activation refused"
        );
    }
    register_gated(fenced, action)
}

fn register_gated(gates: FeatureGates, action: JobAction) -> Option<Job> {
    gates.automations.then(|| job(action))
}

/// Ten occurrences at the 5 s REST timeout, each with a possible orphan
/// delete, stay well inside the attempt timeout.
fn job(action: JobAction) -> Job {
    let cadence = Duration::from_millis(SCHEDULER_TICK_MS);
    Job {
        name: NAMES[0],
        cadence,
        cadence_source: None,
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
    remind_event_attendees(pool, rest, guild, &clock).await?;
    if audit_failed {
        Err(ErrorClass::Database)
    } else {
        Ok(())
    }
}

/// Event reminders share this supervised, activation-fenced ticker, but are
/// optional one-attempt effects. Never recycle their pre-wire audit fence,
/// including after a definitive refusal, rendering failure or lost receipt.
async fn remind_event_attendees(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    clock: &impl Fn() -> u64,
) -> Result<(), ErrorClass> {
    use two_bot_core::event_reminder_store::{begin_event_reminder_delivery, due_event_reminders};

    for event in due_event_reminders(pool, guild, clock())
        .await
        .map_err(database)?
    {
        let now = clock();
        if !begin_event_reminder_delivery(pool, guild, &event, now)
            .await
            .map_err(database)?
        {
            continue;
        }
        let (outcome, reason) = if let Some(content) = event_reminder_content(guild, &event) {
            match rest
                .post_message_confirmed(&event.channel_id, &content, &random_nonce())
                .await
            {
                Ok(_) => ("posted", None),
                Err(error) => (
                    if error.proves_no_effect() || error.is_admission_blocked() {
                        "failed"
                    } else {
                        "delivery_uncertain"
                    },
                    Some(discord_error_name(&error)),
                ),
            }
        } else {
            ("failed", Some("invalid_or_oversized_reminder"))
        };
        // Only the result is appended; the attempt fact remains immutable.
        // No attendee ids, names or rendered content enter diagnostics/audits.
        let input = ScheduledAuditInput {
            guild_id: guild.to_owned(),
            actor_id: None,
            action: "rsvp.reminder".to_owned(),
            target_key: Some(event.event_id),
            outcome: outcome.to_owned(),
            reason: reason.map(str::to_owned),
        };
        audit_scheduled(pool, &input, &format_iso_ms(now), &random_id())
            .await
            .map_err(database)?;
    }
    Ok(())
}

fn event_reminder_content(
    guild: &str,
    event: &two_bot_core::event_reminder_store::EventReminder,
) -> Option<String> {
    use two_bot_core::message_safety::{text_len, truncate, CONTENT_LIMIT};
    use two_bot_core::rsvp::{is_snowflake, MAX_RSVPS_PER_EVENT};

    let count = usize::try_from(event.attendee_count).ok()?;
    if event.attendees.is_empty()
        || event.attendees.len() != count.min(MAX_RSVPS_PER_EVENT as usize + 1)
        || !is_snowflake(&event.event_id)
        || !is_snowflake(&event.channel_id)
        || event.attendees.iter().any(|id| !is_snowflake(id))
    {
        return None;
    }
    // Do not split into a burst or truncate halfway through an attendee list:
    // an oversized but otherwise valid reminder falls back to one bounded,
    // mention-free notice instead of silence. Size the full rendering with the
    // untruncated name: a huge name forces the fallback path even when a
    // truncated name plus the roster would fit.
    if event.attendees.len() <= MAX_RSVPS_PER_EVENT as usize {
        let neutral = neutralize_scheduled_body(&event.name);
        let attendees = event
            .attendees
            .iter()
            .map(|id| format!("<@{id}>"))
            .collect::<Vec<_>>()
            .join(" ");
        let full = format!("Event reminder: {neutral} starts soon.\n{attendees}");
        if text_len(&full) <= CONTENT_LIMIT {
            let name = truncate(&neutral, 200);
            let content = format!("Event reminder: {name} starts soon.\n{attendees}");
            return Some(content);
        }
    }
    event_reminder_fallback(guild, event)
}

/// One bounded, mention-free notice for a valid reminder whose full-mention
/// rendering would exceed `CONTENT_LIMIT` or whose roster exceeds the RSVP
/// bound. Neutral event facts only: bounded name, attendee count, start time
/// and event link. Never a partial roster, never a mention ping, never content
/// from an incomplete source (callers validate first; empty rosters fail
/// closed above). The caller posts this through the same committed-attempt
/// path, so the fallback counts as the one delivery attempt.
fn event_reminder_fallback(
    guild: &str,
    event: &two_bot_core::event_reminder_store::EventReminder,
) -> Option<String> {
    use two_bot_core::message_safety::{text_len, truncate, CONTENT_LIMIT};

    // Break literal "<@" after mention neutralization so a hostile name cannot
    // smuggle a mention-shaped ping into the mention-free fallback. The framing
    // itself carries no mentions and no attendee ids.
    let name = neutralize_scheduled_body(&event.name).replace("<@", "<\u{200b}@");
    let count = event.attendee_count;
    let attendees = if count == 1 {
        "1 attendee".to_owned()
    } else {
        format!("{count} attendees")
    };
    let head = "Event reminder: ";
    let tail = format!(
        " starts at {} ({attendees}).\nSee https://discord.com/events/{guild}/{} for details.",
        event.starts_at, event.event_id
    );
    let budget = CONTENT_LIMIT.saturating_sub(text_len(head) + text_len(&tail));
    if budget == 0 {
        // Only pathological framing (an unbounded stored start time) lands
        // here; fail closed rather than emit an unbounded notice.
        return None;
    }
    let content = format!("{head}{}{tail}", truncate(&name, budget));
    (text_len(&content) <= CONTENT_LIMIT).then_some(content)
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
        if self.row.delivery_uncertain {
            // A reclaimed nonce is not permission to POST beyond Discord's
            // short deduplication window. No history/absence heuristic is used.
            self.warn_uncertain();
            self.audit(
                &OccurrenceOutcome::DeliveryUncertain,
                Some("scheduled_delivery_uncertain"),
                audit_failed,
            )
            .await;
            return Ok(Step::Next);
        }
        match begin_scheduled_delivery(
            self.pool,
            &self.row.guild_id,
            &self.row.id,
            self.claim_token,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.audit(&OccurrenceOutcome::StaleCompletion, None, audit_failed)
                    .await;
                return Ok(Step::Next);
            }
            Err(error) => {
                self.audit(
                    &OccurrenceOutcome::DeliveryUncertain,
                    Some("database"),
                    audit_failed,
                )
                .await;
                return Err(database(error));
            }
        }
        // Defuse legacy bodies, preserve benign text and the closed mention
        // policy, and truncate surrogate-safely after neutralization.
        let content = two_bot_core::message_safety::truncate(
            &neutralize_scheduled_body(&self.row.body),
            two_bot_core::message_safety::CONTENT_LIMIT,
        );
        match self
            .rest
            .post_message_confirmed(
                &self.row.channel_id,
                &content,
                self.row.occurrence_nonce.as_deref().unwrap_or_default(),
            )
            .await
        {
            Ok(message_id) => self.record_post(Some(message_id), audit_failed).await,
            Err(error) => self.record_failure(&error, audit_failed).await,
        }
    }

    fn warn_uncertain(&self) {
        tracing::warn!(
            guild = %self.row.guild_id,
            schedule = %self.row.id,
            code = "scheduled_delivery_uncertain",
            next_action = "inspect_channel_before_cancel_or_replace",
            "scheduled occurrence held; no automatic resend or advancement"
        );
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
                // posting: remove the orphan rather than let it survive. A
                // refused or timed-out delete is surfaced in the audit reason
                // (and a structured warning) instead of reported as a clean
                // cleanup; the orphan is left alone, never retried here or
                // re-posted.
                let cleanup_reason = match &message_id {
                    Some(message_id) => match self.delete(message_id).await {
                        Ok(()) => None,
                        Err(error) => {
                            let reason = discord_error_name(&error);
                            tracing::warn!(guild = %self.row.guild_id, schedule = %self.row.id, reason, "scheduled stale-message cleanup failed");
                            Some(reason)
                        }
                    },
                    None => None,
                };
                self.audit(
                    &OccurrenceOutcome::StaleCompletion,
                    cleanup_reason,
                    audit_failed,
                )
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
                // Only confirmed cleanup disarms the occurrence. Otherwise
                // preserve both identity and the pre-wire fence, including if
                // this retry write fails. Always attempt the audit even when
                // storage fails or a changed generation rejects the write.
                if !cleaned {
                    self.warn_uncertain();
                }
                let retained = match retry_scheduled(
                    self.pool,
                    &self.row.guild_id,
                    &self.row.id,
                    self.claim_token,
                    self.now_iso,
                    !cleaned,
                )
                .await
                {
                    Ok(retained) => retained,
                    Err(error) => {
                        self.audit(
                            &OccurrenceOutcome::PostedUnrecorded { cleaned },
                            Some(reason),
                            audit_failed,
                        )
                        .await;
                        return Err(database(error));
                    }
                };
                let outcome = if retained || cleaned {
                    OccurrenceOutcome::PostedUnrecorded { cleaned }
                } else {
                    OccurrenceOutcome::StaleCompletion
                };
                self.audit(&outcome, Some(reason), audit_failed).await;
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
        let uncertain = !error.proves_no_effect() && !error.is_admission_blocked();
        if uncertain {
            self.warn_uncertain();
        }
        if uncertain || post_failure_retryable(failure_status(error)) {
            let retry_at_ms = self.now.saturating_add(clamp_retry_delay_ms(None));
            let outcome = if uncertain {
                OccurrenceOutcome::DeliveryUncertain
            } else {
                OccurrenceOutcome::Retryable { retry_at_ms }
            };
            let retained = match retry_scheduled(
                self.pool,
                &self.row.guild_id,
                &self.row.id,
                self.claim_token,
                &format_iso_ms(retry_at_ms),
                uncertain,
            )
            .await
            {
                Ok(retained) => retained,
                Err(error) => {
                    self.audit(&outcome, Some(reason), audit_failed).await;
                    return Err(database(error));
                }
            };
            if retained {
                self.audit(&outcome, Some(reason), audit_failed).await;
            } else {
                // The claim went stale (the definition was replaced or
                // cancelled while the retryable post was in flight): the
                // re-queue lands nowhere, so audit the stale completion with
                // the failure reason instead of vanishing silently.
                self.audit(
                    &OccurrenceOutcome::StaleCompletion,
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
        let completed = match complete_run(
            self.pool,
            &self.row.guild_id,
            &self.row.id,
            self.now_iso,
            None,
            self.claim_token,
        )
        .await
        {
            Ok(completed) => completed,
            Err(error) => {
                tracing::warn!(guild = %self.row.guild_id, schedule = %self.row.id, reason = store_error_name(&error), "scheduled failure not recorded");
                self.audit(
                    &OccurrenceOutcome::FailedPermanent,
                    Some(reason),
                    audit_failed,
                )
                .await;
                return Err(ErrorClass::Database);
            }
        };
        if completed.is_some() {
            self.audit(
                &OccurrenceOutcome::FailedPermanent,
                Some(reason),
                audit_failed,
            )
            .await;
        } else {
            // The claim went stale (the definition was replaced or cancelled
            // while Discord refused the post): the completion lands nowhere,
            // so audit the stale completion with the refusal reason instead
            // of vanishing silently.
            self.audit(
                &OccurrenceOutcome::StaleCompletion,
                Some(reason),
                audit_failed,
            )
            .await;
        }
        Ok(Step::Next)
    }

    async fn delete(&self, message_id: &str) -> Result<(), DiscordError> {
        self.rest
            .delete_message_confirmed(&self.row.channel_id, message_id, ORPHAN_REASON)
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
                reason = db_class(&error),
                "scheduled audit write failed"
            );
        }
    }
}

fn database(error: sqlx::Error) -> ErrorClass {
    tracing::warn!(reason = db_class(&error), "scheduled message store failed");
    ErrorClass::Database
}

/// The HTTP status legacy `DiscordPostError` would carry: refusals are 4xx,
/// a spent rate limit is 429, and a timeout or unusable answer had none.
fn failure_status(error: &DiscordError) -> Option<u16> {
    match error {
        DiscordError::Rejected(_) => Some(400),
        DiscordError::RateLimited => Some(429),
        // A local guard refusal never reached the wire and clears with time
        // (breaker closes, global pause ends), so it re-queues like a timeout.
        DiscordError::Timeout | DiscordError::Unavailable(_) | DiscordError::Guard(_) => None,
    }
}

/// Fixed audit reasons (legacy `safeErrorName`): never Discord's text.
fn discord_error_name(error: &DiscordError) -> &'static str {
    match error {
        DiscordError::Rejected(_) => "discord_rejected",
        DiscordError::RateLimited => "discord_rate_limited",
        DiscordError::Timeout => "discord_timeout",
        DiscordError::Unavailable(_) => "discord_unavailable",
        DiscordError::Guard(_) => "discord_guard_refused",
    }
}

fn store_error_name(error: &ScheduledStoreError) -> &'static str {
    match error {
        ScheduledStoreError::CorruptNextRunAt { .. } => "corrupt_next_run_at",
        ScheduledStoreError::Sql(_) => "database",
    }
}

#[cfg(test)]
mod event_reminder_review_regressions {
    use super::*;
    use two_bot_core::event_reminder_store::{
        begin_event_reminder_delivery, due_event_reminders, EventReminder,
    };
    use two_bot_core::message_safety::{contains_mass_mention, text_len, CONTENT_LIMIT};
    use two_bot_testsupport::TestDatabase;

    const GUILD: &str = "3333";
    const EVENT: &str = "1546451670500642999";
    const CHANNEL: &str = "1546451670500642111";
    const NOW: u64 = 1_790_000_000_000;

    async fn overflow_fixture() -> Option<TestDatabase> {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI must supply the guarded disposable database"
            );
            eprintln!("SKIP reminder overflow regression: TWO_TEST_DATABASE_URL is not set");
            return None;
        };
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create disposable reminder database");
        sqlx::query(
            "INSERT INTO scheduled_events
             (guild_id, event_id, name, starts_at, channel_id, status, updated_at)
             VALUES ($1, $2, 'Overflow', $3, $4, 'scheduled', $5)",
        )
        .bind(GUILD)
        .bind(EVENT)
        .bind(format_iso_ms(NOW + 900_000))
        .bind(CHANNEL)
        .bind(format_iso_ms(NOW))
        .execute(db.pool())
        .await
        .expect("seed due event");
        // Bypass RSVP admission to exercise overflow, as legacy/direct rows can.
        // Only the first 1,500 rows qualify; declined/foreign rows must not count.
        sqlx::query(
            "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
             SELECT $1, $2, (1546451670500600000::bigint + n)::text,
                    CASE WHEN n > 1500 THEN 'declined'
                         WHEN n % 2 = 0 THEN 'going' ELSE 'interested' END, now()
               FROM generate_series(1, 1510) n
             UNION ALL SELECT '7777', $2, '1546451670500642888', 'going', now()
             UNION ALL SELECT $1, '1546451670500643001', '1546451670500642888', 'going', now()",
        )
        .bind(GUILD)
        .bind(EVENT)
        .execute(db.pool())
        .await
        .expect("seed oversized RSVP source with excluded rows");
        Some(db)
    }

    async fn selected(pool: &PgPool) -> EventReminder {
        let mut events = due_event_reminders(pool, GUILD, NOW)
            .await
            .expect("read due reminder");
        assert_eq!(events.len(), 1);
        events.pop().expect("one due reminder")
    }

    #[tokio::test]
    async fn overflow_fallback_counts_the_whole_eligible_roster() {
        let Some(db) = overflow_fixture().await else {
            return;
        };
        let event = selected(db.pool()).await;
        assert_eq!(event.attendees.len(), 1001, "member reads stay capped");
        let content = event_reminder_content(GUILD, &event).expect("overflow fallback");
        assert!(content.contains("(1500 attendees)"), "{content}");
        assert!(!content.contains("<@"));
        assert!(text_len(&content) <= CONTENT_LIMIT);
        db.close().await.expect("verified teardown");
    }

    #[tokio::test]
    async fn overflow_count_changes_beyond_the_capped_prefix_reject_stale_admission() {
        let Some(db) = overflow_fixture().await else {
            return;
        };
        let event = selected(db.pool()).await;
        let writer = db
            .independent_pool()
            .await
            .expect("independent RSVP writer");
        sqlx::query(
            "INSERT INTO event_rsvps (guild_id, event_id, user_id, status, responded_at)
             VALUES ($1, $2, '9999999999999999999', 'going', now())",
        )
        .bind(GUILD)
        .bind(EVENT)
        .execute(&writer)
        .await
        .expect("add attendee beyond capped prefix");
        let fresh = selected(db.pool()).await;
        assert_eq!(event.attendees, fresh.attendees, "prefix did not change");
        assert!(
            !begin_event_reminder_delivery(db.pool(), GUILD, &event, NOW)
                .await
                .expect("reject stale total")
        );
        let attempts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM automation_audit_log WHERE action = 'rsvp.reminder_attempt'",
        )
        .fetch_one(db.pool())
        .await
        .expect("read attempt count");
        assert_eq!(attempts, 0, "rejection cannot consume the attempt");
        let content = event_reminder_content(GUILD, &fresh).expect("fresh overflow fallback");
        assert!(content.contains("(1501 attendees)"), "{content}");
        assert!(begin_event_reminder_delivery(db.pool(), GUILD, &fresh, NOW)
            .await
            .expect("admit fresh total"));
        assert!(
            !begin_event_reminder_delivery(db.pool(), GUILD, &fresh, NOW)
                .await
                .expect("refuse duplicate attempt")
        );
        writer.close().await;
        db.close().await.expect("verified teardown");
    }

    #[test]
    fn inconsistent_attendee_totals_fail_closed() {
        let mut event = EventReminder {
            event_id: EVENT.to_owned(),
            name: "Games night".to_owned(),
            starts_at: format_iso_ms(NOW + 900_000),
            channel_id: CHANNEL.to_owned(),
            attendees: vec!["1546451670500642666".to_owned()],
            attendee_count: 1,
        };
        assert!(event_reminder_content(GUILD, &event).is_some());
        for count in [-1, 0, 2, 1500] {
            event.attendee_count = count;
            assert!(event_reminder_content(GUILD, &event).is_none(), "{count}");
        }
    }

    #[test]
    fn fallback_defuses_hostile_names_before_executor_sanitization() {
        let event = EventReminder {
            event_id: EVENT.to_owned(),
            name: format!(
                "Party <@1546451670500642888> <@&1546451670500642777> @everyone @here discord.gg/x DISCORD.COM/INVITE/y discordapp.com/invite/z {}",
                "🚀".repeat(3000)
            ),
            starts_at: format_iso_ms(NOW + 900_000),
            channel_id: CHANNEL.to_owned(),
            attendees: vec!["1546451670500642666".to_owned()],
            attendee_count: 1,
        };
        // Inspect the renderer itself: the executor cannot mask a regression.
        let content = event_reminder_content(GUILD, &event).expect("huge name falls back");
        assert!(content.contains("1 attendee"));
        assert!(content.contains(&format!("https://discord.com/events/{GUILD}/{EVENT}")));
        assert!(!content.contains("<@"), "literal mention shape escaped");
        assert!(!contains_mass_mention(&content));
        for host in ["discord.gg", "discord.com/invite", "discordapp.com/invite"] {
            assert!(!content.to_ascii_lowercase().contains(host), "{host}");
        }
        assert!(text_len(&content) <= CONTENT_LIMIT);
        assert!(content.contains('🚀'), "bounded name must retain safe text");
    }
}
