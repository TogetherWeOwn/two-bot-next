//! Whole-tick event-voice preflight. The guarded continuation owns the scan,
//! durable claims and room operations; it is never called on a failed preflight.

use super::*;
use std::{future::Future, time::Instant};
use two_bot::{
    event_voice_category::{
        CategoryReader, CategorySnapshot, EventVoiceCategoryGate, LiveCategoryReader,
        PreflightDecision, StaffAlert, StaffAlertSink,
    },
    voice_rooms::LiveGuild,
};
use two_bot_discord::voice_rooms::RoomHttpError;

pub(super) struct Settings {
    pub category: Option<u64>,
    pub staff_channel: Option<String>,
}

pub(super) struct Tick {
    pub gate: JobGate,
    category: Mutex<EventVoiceCategoryGate>,
}

impl Default for Tick {
    fn default() -> Self {
        Self {
            gate: JobGate::default(),
            category: Mutex::new(EventVoiceCategoryGate::new(None)),
        }
    }
}

struct Reader<'a> {
    rest: &'a ActionExecutor,
    live: Option<LiveGuild>,
}

impl CategoryReader for Reader<'_> {
    async fn category_grant(
        &self,
        guild: u64,
        category: u64,
    ) -> Result<CategorySnapshot, RoomHttpError> {
        let live = self.live.as_ref().ok_or(RoomHttpError::Cancelled)?;
        LiveCategoryReader::new(self.rest, live)
            .category_grant(guild, category)
            .await
    }
}

#[derive(Default)]
struct Alerts(std::sync::Mutex<Vec<StaffAlert>>);

impl StaffAlertSink for Alerts {
    fn send_staff_alert(&self, alert: StaffAlert) {
        // Log before any await: cancellation or a refused Discord delivery
        // cannot erase the episode's evidence. No remote body or config value.
        tracing::warn!(
            reason = alert.reason.as_str(),
            category_id = alert.category,
            "event_voice_staff_alert"
        );
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(alert);
    }
}

impl Tick {
    pub async fn run<F, Fut>(
        &self,
        rest: &ActionExecutor,
        guild: u64,
        live: Option<LiveGuild>,
        settings: Settings,
        shutdown: &watch::Receiver<bool>,
        action: F,
    ) -> Result<(), ErrorClass>
    where
        F: FnOnce(u64) -> Fut + Send,
        Fut: Future<Output = Result<(), ErrorClass>> + Send,
    {
        // Acquire before preflight, database acquisition, or the scan. Do not
        // reacquire in the continuation: this guard lives across its awaits.
        let Some(_held) = self.gate.try_acquire() else {
            return Ok(());
        };
        tokio::select! {
            biased;
            _ = server::shutdown_requested(shutdown.clone()) => Ok(()),
            result = async {
                let alerts = Alerts::default();
                let decision = {
                    let mut gate = self.category.lock().await;
                    gate.reconfigure(settings.category);
                    gate.preflight(&Reader { rest, live }, &alerts, guild, Instant::now()).await
                };
                let queued = alerts.0.into_inner().unwrap_or_else(|p| p.into_inner());
                if let Some(channel) = settings.staff_channel {
                    let delivery = crate::raid_runtime::RaidDelivery::new(rest.clone(), guild);
                    for alert in queued {
                        let content = format!(
                            "Event voice is inactive: {}. Check the configured event voice category and the bot's View Channel / Manage Channels permissions.",
                            alert.reason.as_str()
                        );
                        // Existing staff delivery verifies guild + View/Send,
                        // enforces empty mentions and bounds the attempt. One
                        // attempt per episode: uncertainty must not spam staff.
                        let result = delivery.post_staff_message(&channel, &content).await;
                        if !matches!(result, crate::raid_runtime::Delivery::Posted) {
                            tracing::warn!(reason = alert.reason.as_str(), "event_voice_staff_alert_undelivered");
                        }
                    }
                }
                if publication_stopped(shutdown) {
                    return Ok(());
                }
                match decision {
                    PreflightDecision::Ready { category } => action(category).await,
                    PreflightDecision::Waiting { .. } => Ok(()),
                }
            } => result,
        }
    }
}
