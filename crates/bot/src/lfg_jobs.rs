//! Bounded LFG lifecycle maintenance through the shared jobs supervisor.

use std::{sync::Arc, time::Duration};
use tokio::sync::OnceCell;
use two_bot_core::FeatureGates;
use two_bot_discord::lfg_interactions::{LfgError, LfgInteractions, LFG_SWEEP_MAX_ROWS};

use crate::{
    activation::BootActivation,
    jobs::{self, ErrorClass, Job},
    website_jobs::Context,
};

pub(crate) const NAME: &str = "lfg_sweep";
const CADENCE: Duration = Duration::from_secs(60);

/// Registration only narrows the existing announcement gate. The supervisor
/// owns cadence, single-flight, timeout and shutdown; no private task is spawned.
pub(crate) fn register(context: Arc<Context>, activation: &BootActivation) -> Option<Job> {
    let gates = FeatureGates::from_env().ok()?;
    if !activation.constrain_features(gates).announcements {
        return None;
    }
    // Reuse the website pool and governed executor. Keep one maintenance
    // service/identity per registration, never one connection pool per tick.
    // Cross-runtime ordering uses the same DB advisory lock as interactions.
    let service = Arc::new(OnceCell::<LfgInteractions>::new());
    let bot_user = Arc::new(OnceCell::<u64>::new());
    Some(Job {
        name: NAME,
        cadence: CADENCE,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(CADENCE, rand::random()),
        timeout: Duration::from_secs(120),
        action: Arc::new(move || {
            let context = context.clone();
            let service = service.clone();
            let bot_user = bot_user.clone();
            Box::pin(async move {
                let service = service
                    .get_or_try_init(|| async {
                        context
                            .pool()
                            .await
                            .map(|pool| LfgInteractions::new(pool.clone()))
                    })
                    .await?;
                let bot_user = bot_user
                    .get_or_try_init(|| async {
                        context
                            .rest
                            .current_bot_user_id()
                            .await
                            .map_err(|_| ErrorClass::Rest)
                    })
                    .await?;
                service
                    .sweep_lifecycle(
                        &context.rest,
                        &context.guild,
                        *bot_user,
                        time::OffsetDateTime::now_utc(),
                        LFG_SWEEP_MAX_ROWS,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| match error {
                        LfgError::Database(_) | LfgError::Audit(_) | LfgError::Definition(_) => {
                            ErrorClass::Database
                        }
                        LfgError::Discord(_) => ErrorClass::Rest,
                        LfgError::Uncertain => ErrorClass::RecoveryRequired,
                        LfgError::Invalid(_) | LfgError::Busy | LfgError::CoolingDown => {
                            ErrorClass::Configuration
                        }
                    })
            })
        }),
    })
}
