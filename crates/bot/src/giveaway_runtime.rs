//! Giveaway administration and member entry over the shared router/executor.
//!
//! Thin production shell over [`two_bot_discord::giveaway`]: the bot
//! `CommandRuntime` (defer-first) and the supervised countdown/draw jobs call
//! through here, while all behavior lives in the shared module so the live
//! dispatch path and the timed-draw seam cannot drift apart.

use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_discord::ActionExecutor;

use crate::jobs::ErrorClass;

pub(crate) const REFRESH_JOB: &str = "giveaway_countdown";
pub(crate) const DRAW_JOB: &str = "giveaway_draw";
const DRAW_CADENCE_SECS: u64 = 15;
const DRAW_TIMEOUT_SECS: u64 = 300;

pub(crate) struct GiveawayRuntime {
    inner: two_bot_discord::giveaway::GiveawayRuntime,
}

impl GiveawayRuntime {
    pub(crate) fn new(pool: PgPool, executor: ActionExecutor) -> Self {
        Self {
            inner: two_bot_discord::giveaway::GiveawayRuntime::new(pool, executor),
        }
    }

    /// Called only after the shared router authorized and deferred the command.
    pub(crate) async fn command(&self, interaction: &Interaction) {
        self.inner.command(interaction).await;
    }

    /// Called only after guild/gate routing and a successful ephemeral defer.
    pub(crate) async fn enter(&self, interaction: &Interaction, custom_id: &str) {
        self.inner.enter(interaction, custom_id).await;
    }

    /// Countdown text itself is rendered by Discord. At most one edit per
    /// durable 1h/10m/1m/30s bucket, never one edit per supervisor tick.
    pub(crate) async fn refresh(&self, guild: &str) -> Result<(), ErrorClass> {
        self.inner.refresh(guild).await.map_err(job_class)
    }

    /// One due-scan tick: claim every due giveaway and draw under the claim,
    /// then re-announce every drawn-but-unannounced row from its stored draw.
    pub(crate) async fn tick_draw(&self, guild: &str) -> Result<(), ErrorClass> {
        self.inner.tick(guild).await.map(|_| ()).map_err(job_class)
    }
}

fn job_class(error: two_bot_discord::giveaway::GiveawayJobError) -> ErrorClass {
    match error {
        two_bot_discord::giveaway::GiveawayJobError::Database(_) => ErrorClass::Database,
        two_bot_discord::giveaway::GiveawayJobError::Rest(_)
        | two_bot_discord::giveaway::GiveawayJobError::Announce => ErrorClass::Rest,
    }
}

/// Reuse the existing job supervisor, identity fence, pool and REST admission.
pub(crate) fn register(
    context: std::sync::Arc<crate::website_jobs::Context>,
    activation: &crate::activation::BootActivation,
) -> Option<crate::jobs::Job> {
    let gates = two_bot_core::FeatureGates::from_env().ok()?;
    if !activation.constrain_features(gates).automations {
        return None;
    }
    let cadence = std::time::Duration::from_secs(15);
    Some(crate::jobs::Job {
        name: REFRESH_JOB,
        cadence,
        cadence_source: None,
        startup_jitter: crate::jobs::startup_jitter(cadence, rand::random()),
        timeout: std::time::Duration::from_secs(120),
        action: std::sync::Arc::new(move || {
            let context = context.clone();
            Box::pin(async move {
                GiveawayRuntime::new(context.pool().await?.clone(), context.rest.clone())
                    .refresh(&context.guild)
                    .await
            })
        }),
    })
}

/// The due-scan draw job on the shared supervisor: 15 s cadence (missed ticks
/// Skip, supervisor-wide), a 300 s timeout covering member re-validation, and
/// the same automations gate as the countdown job. Draws commit under an
/// expiring claim lease; announcements retry until marked.
pub(crate) fn register_draw(
    context: std::sync::Arc<crate::website_jobs::Context>,
    activation: &crate::activation::BootActivation,
) -> Option<crate::jobs::Job> {
    let gates = two_bot_core::FeatureGates::from_env().ok()?;
    if !activation.constrain_features(gates).automations {
        return None;
    }
    let cadence = std::time::Duration::from_secs(DRAW_CADENCE_SECS);
    Some(crate::jobs::Job {
        name: DRAW_JOB,
        cadence,
        cadence_source: None,
        startup_jitter: crate::jobs::startup_jitter(cadence, rand::random()),
        timeout: std::time::Duration::from_secs(DRAW_TIMEOUT_SECS),
        action: std::sync::Arc::new(move || {
            let context = context.clone();
            Box::pin(async move {
                GiveawayRuntime::new(context.pool().await?.clone(), context.rest.clone())
                    .tick_draw(&context.guild)
                    .await
            })
        }),
    })
}
