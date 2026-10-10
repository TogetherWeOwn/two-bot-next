//! Gateway wiring for the shared automod activation.
//!
//! The serial dispatch worker calls [`process`] once per translated delivery,
//! in gateway order and before the funnel, then hands the funnel disposition
//! to `OrderedLevelingPipeline::collect_at_with_message_disposition` exactly once. There is no
//! private client, router or timer: Discord reads and mutations go through the
//! command runtime's [`ActionExecutor`], and repeat history expires on the
//! shared periodic-job supervisor ([`expiry_job`]).
//!
//! The funnel's writes commit with the gateway checkpoint, so every dispatch
//! reaching [`process`] is not yet in the funnel. A replay after a crash
//! between the claim write and that commit therefore restores the funnel once
//! from the stored receipt (see `Activation::uncommitted_disposition`); the
//! Discord effect is never resent. A dry-run to enforce change owns a new
//! claim, and the same rule keeps the funnel once-per-message.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sqlx::PgPool;
use tracing::warn;
use two_bot_core::automod_runtime::{
    AutomodClaimLedger, AutomodRuntime, AutomodScope, FunnelDisposition, MessageDelivery,
    MessageDeliveryKind,
};
use two_bot_core::automod_store::AutomodStore;
use two_bot_core::moderation::ModerationGates;
use two_bot_core::AutomodConfig;
use two_bot_discord::automod::{partial_edit_delivery, PartialEdit};
use two_bot_discord::automod_activation::{
    Activation, ActivationOutcome, AutomodActivation, AutomodFacts, RestAutomodFacts,
};
use two_bot_discord::ActionExecutor;

use crate::jobs::{self, Job};

pub type ProductionAutomod = AutomodActivation<AutomodStore, RestAutomodFacts>;

/// Set once by the gateway task; read by the maintenance job.
pub type Slot = Arc<OnceLock<Arc<ProductionAutomod>>>;

pub(crate) const JOB_NAME: &str = "automod_expiry";
const EXPIRY_CADENCE: Duration = Duration::from_secs(60);

/// Bound on one delivery's claim writes, REST reads and mutations. The worker's
/// own deadline (20 s including the checkpoint) is fatal; this keeps a slow
/// Discord from taking the shard down. A timed-out claim stays unsettled for
/// recorded reconciliation and is never resent.
pub(crate) const PROCESS_MAX: Duration = Duration::from_secs(12);

/// Gates resolved from configuration. Owen and the protected roles come from
/// the moderation settings, never from `TWO_AUTOMOD_ENFORCE`.
#[derive(Debug)]
pub(crate) struct Resolved {
    config: AutomodConfig,
    scope: AutomodScope,
    owen_user_id: String,
    protected_role_ids: HashSet<String>,
}

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// `Ok(None)` when automod is off. An enabled but invalid configuration is an
/// error: silently running unmoderated would be worse than refusing to start.
/// Live-guild approval is externally supplied and absent here, so only the
/// staging guild is ever in scope.
pub(crate) fn resolve(
    vars: &HashMap<String, String>,
    guild_id: u64,
) -> Result<Option<Resolved>, &'static str> {
    let config = AutomodConfig::from_map(vars).map_err(|_| "TWO_AUTOMOD configuration invalid")?;
    if !config.enabled {
        return Ok(None);
    }
    let gates = ModerationGates::from_map(vars)
        .map_err(|_| "TWO_MODERATION_PROTECTED_ROLE_IDS configuration invalid")?;
    if !is_snowflake(&gates.owen_user_id) {
        return Err("TWO_AUTOMOD=1 requires TWO_OWEN_USER_ID");
    }
    Ok(Some(Resolved {
        config,
        scope: AutomodScope {
            guild_id: guild_id.to_string(),
            live_approved: false,
        },
        owen_user_id: gates.owen_user_id,
        protected_role_ids: gates.protected_role_ids,
    }))
}

pub(crate) fn build(
    resolved: Resolved,
    pool: PgPool,
    executor: ActionExecutor,
) -> Arc<ProductionAutomod> {
    let facts = RestAutomodFacts::new(
        executor.clone(),
        resolved.owen_user_id,
        resolved.protected_role_ids,
    );
    Arc::new(AutomodActivation::new(
        AutomodRuntime::new(resolved.config, resolved.scope),
        AutomodStore::new(pool),
        facts,
        executor,
    ))
}

pub(crate) fn receipt_ms(observed_at: &str) -> u64 {
    two_bot_core::parse_iso_millis(observed_at)
        .and_then(|ms| u64::try_from(ms).ok())
        .unwrap_or(0)
}

/// Decode a raw MESSAGE_UPDATE dispatch BEFORE `twilight_gateway::parse`: a
/// minimal edit omits fields a full Twilight `Message` requires and would fail
/// decoding upstream of enrichment. Any other dispatch is `None`.
pub(crate) fn partial_edit(text: &str, receipt_ms: u64) -> Option<MessageDelivery> {
    if !text.contains("MESSAGE_UPDATE") {
        return None;
    }
    let packet: serde_json::Value = serde_json::from_str(text).ok()?;
    if packet.get("t")?.as_str()? != "MESSAGE_UPDATE" {
        return None;
    }
    let edit = PartialEdit::from_dispatch(packet.get("d")?)?;
    Some(partial_edit_delivery(&edit, receipt_ms))
}

/// What one delivery decided: `funnel` is its single call into the funnel and
/// `trigger` is the verdict prefix triggers act on. A delivery automod did not
/// inspect, or whose completion was not recorded, keeps its funnel disposition
/// but its trigger verdict is capture-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkerVerdict {
    pub(crate) funnel: FunnelDisposition,
    pub(crate) trigger: FunnelDisposition,
}

fn verdict_of(activation: &Activation, kind: MessageDeliveryKind) -> WorkerVerdict {
    let funnel = activation.uncommitted_disposition(kind);
    let trigger = match activation.outcome {
        ActivationOutcome::Bypassed | ActivationOutcome::Retained(_) => {
            FunnelDisposition::CaptureOnly
        }
        _ => funnel,
    };
    WorkerVerdict { funnel, trigger }
}

/// Run one delivery through the activation. A timeout never becomes acceptance:
/// a create keeps raw capture only.
pub(crate) async fn process<L: AutomodClaimLedger, F: AutomodFacts>(
    activation: &AutomodActivation<L, F>,
    delivery: MessageDelivery,
    at_iso: &str,
) -> WorkerVerdict {
    let kind = delivery.kind;
    match tokio::time::timeout(PROCESS_MAX, activation.process(delivery, at_iso)).await {
        Ok(result) => verdict_of(&result, kind),
        Err(_) => {
            warn!("automod delivery timed out; claim left for reconciliation");
            let capture = kind.funnel(true);
            WorkerVerdict {
                funnel: capture,
                trigger: capture,
            }
        }
    }
}

/// Text automations (sticky and friends) run only for an accepted create: a
/// matched, unavailable or timed-out create is rejected for them too.
pub(crate) fn runs_text_automations(disposition: Option<FunnelDisposition>) -> bool {
    disposition.is_none_or(|disposition| disposition == FunnelDisposition::Accept)
}

pub(crate) fn enabled() -> bool {
    std::env::var("TWO_AUTOMOD").is_ok_and(|value| value == "1")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// The shared maintenance tick: expire idle repeat history. No I/O, no
/// private timer; a gateway that has not built the activation yet is a no-op.
pub(crate) fn expiry_job(slot: Slot) -> Job {
    Job {
        name: JOB_NAME,
        cadence: EXPIRY_CADENCE,
        startup_jitter: jobs::startup_jitter(EXPIRY_CADENCE, rand::random()),
        timeout: Duration::from_secs(5),
        action: Arc::new(move || {
            let slot = Arc::clone(&slot);
            Box::pin(async move {
                if let Some(activation) = slot.get() {
                    activation.expire_repeat_history(now_ms());
                }
                Ok(())
            })
        }),
    }
}

#[cfg(test)]
#[path = "automod_gateway_tests.rs"]
mod tests;
