#![cfg(test)]

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

const GUILD: Snowflake = 900;
const CATEGORY: Snowflake = 700;
const MOVED_CATEGORY: Snowflake = 701;
const NEW_CATEGORY: Snowflake = 702;
const EVENT: Snowflake = 800;

struct Reader(Mutex<VecDeque<Result<CategorySnapshot, RoomHttpError>>>);

impl Reader {
    fn new(script: impl IntoIterator<Item = Result<CategorySnapshot, RoomHttpError>>) -> Self {
        Self(Mutex::new(script.into_iter().collect()))
    }
}

impl CategoryReader for Reader {
    async fn category_grant(
        &self,
        guild: Snowflake,
        category: Snowflake,
    ) -> Result<CategorySnapshot, RoomHttpError> {
        assert_eq!(guild, GUILD);
        let result = self.0.lock().unwrap().pop_front().unwrap();
        if let Ok(snapshot) = result {
            assert_eq!(category, snapshot.id);
        }
        result
    }
}

#[derive(Default)]
struct Alerts(Mutex<Vec<StaffAlert>>);

impl StaffAlertSink for Alerts {
    fn send_staff_alert(&self, alert: StaffAlert) {
        self.0.lock().unwrap().push(alert);
    }
}

fn snapshot(permissions: Option<Permissions>) -> CategorySnapshot {
    CategorySnapshot {
        id: CATEGORY,
        bot_permissions: permissions,
        everyone_can_view: true,
    }
}

fn denied_ticks() -> u64 {
    metrics::global()
        .render(None)
        .lines()
        .find_map(|line| {
            line.strip_prefix("two_bot_event_voice_tick_total{outcome=\"perm_denied\"} ")?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn manage_channels_without_view_channel_is_denied() {
    // Removing VIEW_CHANNEL from required_category_grant must not make this Ready.
    let reader = Reader::new([Ok(snapshot(Some(
        Permissions::MANAGE_CHANNELS | Permissions::CONNECT,
    )))]);
    let mut gate = EventVoiceCategoryGate::new(Some(CATEGORY));
    assert_eq!(
        gate.preflight(&reader, &Alerts::default(), GUILD, Instant::now())
            .await,
        PreflightDecision::Waiting {
            reason: EventVoiceTbdReason::PermDenied,
        }
    );
}

#[tokio::test]
async fn weak_grant_records_every_denied_tick_and_alerts_once() {
    // The Ok(snapshot) denial arm must record and alert, not only the HTTP 403 arm.
    for permissions in [Some(Permissions::VIEW_CHANNEL | Permissions::CONNECT), None] {
        let reader = Reader::new((0..3).map(|_| Ok(snapshot(permissions))));
        let alerts = Alerts::default();
        let mut gate = EventVoiceCategoryGate::new(Some(CATEGORY));
        let before = denied_ticks();
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(
                gate.preflight(&reader, &alerts, GUILD, now).await,
                PreflightDecision::Waiting {
                    reason: EventVoiceTbdReason::PermDenied,
                }
            );
        }
        // Other tests also increment this process-global metric; never reset it.
        assert!(denied_ticks() - before >= 3);
        assert_eq!(
            *alerts.0.lock().unwrap(),
            vec![StaffAlert {
                reason: EventVoiceTbdReason::PermDenied,
                category: Some(CATEGORY),
            }]
        );
        assert!(reader.0.lock().unwrap().is_empty());
    }
}

struct Patcher(Mutex<Vec<(Snowflake, Snowflake)>>);

impl EventStatusPatcher for Patcher {
    async fn patch_event_completed(
        &self,
        guild: Snowflake,
        event: Snowflake,
    ) -> Result<(), RoomHttpError> {
        let mut calls = self.0.lock().unwrap();
        calls.push((guild, event));
        match calls.len() {
            1 => Err(RoomHttpError::AccessDenied),
            2 => Ok(()),
            _ => panic!("published patch must not call Discord again"),
        }
    }
}

#[tokio::test]
async fn completed_patch_preserves_guild_and_event_ids_on_retry() {
    // Swapping same-typed guild/event arguments must fail on the first call and retry.
    let patcher = Patcher(Mutex::new(Vec::new()));
    let mut patch = PendingCompletedPatch::for_event(GUILD, EVENT);
    let now = Instant::now();
    assert_eq!(
        patch.ensure_published(&patcher, now).await,
        PatchOutcome::QueuedForRetry
    );
    assert_eq!(*patcher.0.lock().unwrap(), vec![(900, 800)]);
    assert_eq!(
        patch
            .ensure_published(&patcher, now + EVENT_PATCH_RETRY_AFTER)
            .await,
        PatchOutcome::Published
    );
    assert_eq!(*patcher.0.lock().unwrap(), vec![(900, 800), (900, 800)]);
}

#[test]
fn reconfigure_discards_a_move_to_a_different_third_category() {
    // The moved parent and new configuration differ, exposing a stale cached move.
    let mut gate = EventVoiceCategoryGate::new(Some(CATEGORY));
    gate.note_mod_move(MOVED_CATEGORY, false);
    assert_eq!(gate.effective_category(), Some(MOVED_CATEGORY));
    assert!(gate.members_only_link());
    gate.reconfigure(Some(NEW_CATEGORY));
    assert_eq!(gate.configured(), Some(NEW_CATEGORY));
    assert_eq!(gate.effective_category(), Some(NEW_CATEGORY));
    assert_eq!(gate.create_parent(), Some(NEW_CATEGORY));
    assert!(!gate.members_only_link());
}

async fn assert_reconfigure_rearms_alert(error: RoomHttpError, reason: EventVoiceTbdReason) {
    let reader = Reader::new([Err(error), Err(error)]);
    let alerts = Alerts::default();
    let mut gate = EventVoiceCategoryGate::new(Some(CATEGORY));
    let now = Instant::now();
    assert_eq!(
        gate.preflight(&reader, &alerts, GUILD, now).await,
        PreflightDecision::Waiting { reason }
    );
    assert_eq!(alerts.0.lock().unwrap().len(), 1);

    // Same id preserves the latch/backoff; a new denied id is a fresh episode,
    // even without a successful Ready in between and before old backoff expires.
    gate.reconfigure(Some(CATEGORY));
    gate.preflight(&reader, &alerts, GUILD, now).await;
    assert_eq!(alerts.0.lock().unwrap().len(), 1);
    gate.reconfigure(Some(NEW_CATEGORY));
    for _ in 0..3 {
        assert_eq!(
            gate.preflight(&reader, &alerts, GUILD, now).await,
            PreflightDecision::Waiting { reason }
        );
    }
    assert_eq!(
        *alerts.0.lock().unwrap(),
        vec![
            StaffAlert {
                reason,
                category: Some(CATEGORY),
            },
            StaffAlert {
                reason,
                category: Some(NEW_CATEGORY),
            },
        ]
    );
    assert!(reader.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconfigure_rearms_permission_denied_alert_for_new_category() {
    assert_reconfigure_rearms_alert(RoomHttpError::AccessDenied, EventVoiceTbdReason::PermDenied)
        .await;
}

#[tokio::test]
async fn reconfigure_rearms_no_category_alert_for_new_deleted_category() {
    assert_reconfigure_rearms_alert(RoomHttpError::NotFound, EventVoiceTbdReason::NoCategory).await;
}
