//! Timed polls run on the shared Skip-on-missed-ticks supervisor.

use crate::{
    activation::BootActivation,
    jobs::{self, ErrorClass, Job},
    website_jobs::Context,
};
use std::{sync::Arc, time::Duration};
use two_bot_core::{
    ClassifierConfig, FeatureGates, InteractionRouter, ModerationGates, RouterGates, SurfaceFlags,
};
use two_bot_discord::interactions::InteractionRuntime;

pub(crate) const NAME: &str = "polls";

/// The boot identity can only narrow the announcements gate. Invalid config
/// parks both the scheduled surface and its status entry, never defaults on.
pub(crate) fn register(context: Arc<Context>, activation: &BootActivation) -> Option<Job> {
    let features = activation.constrain_features(FeatureGates::from_env().ok()?);
    if !features.announcements {
        return None;
    }
    let guild = context.guild.parse().ok()?;
    let gates = RouterGates::from_slices(
        Some(guild),
        &features,
        &ModerationGates {
            enabled: false,
            owen_user_id: String::new(),
            protected_role_ids: Default::default(),
        },
        SurfaceFlags::default(),
    );
    let runtime = Arc::new(tokio::sync::OnceCell::<InteractionRuntime>::new());
    let cadence = Duration::from_secs(15);
    Some(Job {
        name: NAME,
        cadence,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: Duration::from_secs(120),
        action: Arc::new(move || {
            let context = context.clone();
            let runtime = runtime.clone();
            Box::pin(async move {
                let pool = context.pool().await?;
                let runtime = runtime
                    .get_or_init(|| async {
                        InteractionRuntime::with_router(
                            InteractionRouter::new(gates),
                            pool.clone(),
                            context.rest.clone(),
                            0,
                            ClassifierConfig::default(),
                        )
                    })
                    .await;
                runtime
                    .tick_polls()
                    .await
                    .map_err(|_| ErrorClass::RecoveryRequired)
            })
        }),
    })
}
