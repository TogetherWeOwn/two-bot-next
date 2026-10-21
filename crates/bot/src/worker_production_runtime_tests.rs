#![cfg(test)]
//! Production constructor coverage. Removing a shutdown callback must leave
//! these live observers wedged, not pass by dropping their inbox senders.

use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use two_bot_discord::JoinObserver;

use crate::tuning_test_support as support;
use crate::{containment_runtime, join_risk_runtime, raid_runtime};

const JOIN_BOUND: Duration = Duration::from_secs(5);
const ENV_KEYS: &[&str] = &[
    "TWO_RAID_JOIN_THRESHOLD",
    "TWO_RAID_WINDOW_SECONDS",
    "TWO_ANTI_NUKE",
    "TWO_ANTI_NUKE_DRY_RUN",
    "TWO_ONBOARDING_MODE",
    "TWO_ANTI_NUKE_WINDOW_SECONDS",
    "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS",
    "TWO_ANTI_NUKE_HEAT_THRESHOLD",
    "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
    "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
    "TWO_JOIN_RISK_THRESHOLD",
    "TWO_JOIN_RISK_WINDOW_SECONDS",
    "TWO_BULK_JOIN_WINDOW_UNTIL",
];
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct RestoreEnv(Vec<(&'static str, Option<OsString>)>);

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn with_env(pairs: &[(&str, &str)], test: impl Future<Output = ()>) {
    // Each caller runs alone in a subprocess via the existing isolation
    // helper. Also serialize changes and restore them on a panic, as in the
    // existing boot-setting tests. No lock is held by an async test future.
    let _lock = ENV_LOCK.lock().expect("constructor environment lock");
    let _restore = RestoreEnv(
        ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect(),
    );
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    for (key, value) in pairs {
        assert!(ENV_KEYS.contains(key), "fixture key must be restored");
        std::env::set_var(key, value);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("offline constructor runtime")
        .block_on(test);
}

async fn joined(supervisor: JoinHandle<()>) {
    tokio::time::timeout(JOIN_BOUND, supervisor)
        .await
        .expect("production supervisor must join before its abort budget")
        .expect("production supervisor must not panic");
}

#[test]
fn raid_constructor_retains_worker_until_service_shutdown() {
    if support::isolated_log_test() {
        return;
    }
    with_env(
        &[
            ("TWO_RAID_JOIN_THRESHOLD", "8"),
            ("TWO_RAID_WINDOW_SECONDS", "15"),
        ],
        async {
            let pool = support::closed_pool().await;
            let (executor, trap) = support::no_discord();
            let (shutdown, stopping) = watch::channel(false);
            let (runtime, mut supervisor) =
                raid_runtime::start_from_env(pool, executor, support::GUILD, stopping);
            assert!(futures_util::poll!(&mut supervisor).is_pending());
            assert_eq!(runtime.finished(), 0);
            assert_eq!(runtime.dropped_shutdown(), 0);
            shutdown.send(true).expect("service shutdown");
            joined(supervisor).await;
            // Keep the sender alive through the join: sender-drop must not
            // masquerade as shutdown propagation from the constructor.
            assert_eq!(runtime.finished(), 0);
            assert_eq!(runtime.dropped_shutdown(), 0);
            let weak = Arc::downgrade(&runtime);
            drop(runtime);
            assert!(weak.upgrade().is_none(), "supervisor releases its runtime");
            support::assert_no_discord(&trap);
        },
    );
}

#[test]
fn containment_env_constructor_refuses_disabled_and_wrong_guild_fences() {
    if support::isolated_log_test() {
        return;
    }
    for (pairs, guild) in [
        (vec![], two_bot_cutover::STAGING_GUILD_ID.parse().unwrap()),
        (vec![("TWO_ANTI_NUKE", "1")], support::GUILD),
    ] {
        with_env(&pairs, async {
            let pool = support::closed_pool().await;
            let (executor, trap) = support::no_discord();
            let (_shutdown, stopping) = watch::channel(false);
            assert!(
                containment_runtime::start_from_env(pool, executor, guild, stopping).is_none(),
                "disabled fences must not create an observer or supervisor"
            );
            support::assert_no_discord(&trap);
        });
    }
}

#[test]
fn containment_env_constructor_joins_enabled_safe_dry_run_with_live_observer() {
    if support::isolated_log_test() {
        return;
    }
    with_env(
        &[
            ("TWO_ANTI_NUKE", "1"),
            ("TWO_ANTI_NUKE_DRY_RUN", "1"),
            ("TWO_ANTI_NUKE_WINDOW_SECONDS", "30"),
            ("TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS", "60"),
            ("TWO_ANTI_NUKE_HEAT_THRESHOLD", "8"),
            ("TWO_ANTI_NUKE_TRUSTED_USER_IDS", "123456789012345678"),
            ("TWO_ANTI_NUKE_PROTECTED_USER_IDS", "223456789012345678"),
        ],
        async {
            let pool = support::closed_pool().await;
            let (executor, mock) = support::containment_identity().await;
            let (shutdown, stopping) = watch::channel(false);
            let guild = two_bot_cutover::STAGING_GUILD_ID.parse().unwrap();
            let (observer, mut supervisor) =
                containment_runtime::start_from_env(pool, executor, guild, stopping)
                    .expect("safe dry-run installs containment and retains its supervisor");
            assert!(futures_util::poll!(&mut supervisor).is_pending());
            shutdown.send(true).expect("service shutdown");
            joined(supervisor).await;
            let weak = Arc::downgrade(&observer);
            drop(observer);
            assert!(weak.upgrade().is_none(), "supervisor releases its observer");
            support::assert_only_containment_identity(mock).await;
        },
    );
}

#[test]
fn risk_env_constructor_keeps_raid_only_when_fences_refuse() {
    if support::isolated_log_test() {
        return;
    }
    for (pairs, guild) in [
        (vec![], two_bot_cutover::STAGING_GUILD_ID.parse().unwrap()),
        (vec![("TWO_ANTI_NUKE", "1")], support::GUILD),
    ] {
        with_env(&pairs, async {
            let pool = support::closed_pool().await;
            let (executor, trap) = support::no_discord();
            let (shutdown, stopping) = watch::channel(false);
            let (raid, raid_supervisor) = raid_runtime::start_from_env(
                pool.clone(),
                executor.clone(),
                guild,
                stopping.clone(),
            );
            let expected: Arc<dyn JoinObserver> = raid.clone();
            let (observer, risk_supervisor) =
                join_risk_runtime::chain_from_env(pool, executor, guild, raid, stopping);
            assert!(risk_supervisor.is_none(), "no disabled risk worker");
            assert!(
                Arc::ptr_eq(&observer, &expected),
                "retain the raid observer"
            );
            shutdown.send(true).expect("service shutdown");
            joined(raid_supervisor).await;
            drop((observer, expected));
            support::assert_no_discord(&trap);
        });
    }
}

#[test]
fn risk_env_constructor_joins_both_workers_with_fanout_still_alive() {
    if support::isolated_log_test() {
        return;
    }
    with_env(
        &[
            ("TWO_ANTI_NUKE", "1"),
            ("TWO_ANTI_NUKE_DRY_RUN", "1"),
            ("TWO_ONBOARDING_MODE", "session"),
            ("TWO_JOIN_RISK_THRESHOLD", "8"),
            ("TWO_JOIN_RISK_WINDOW_SECONDS", "30"),
        ],
        async {
            let pool = support::closed_pool().await;
            let (executor, trap) = support::no_discord();
            let (shutdown, stopping) = watch::channel(false);
            let guild = two_bot_cutover::STAGING_GUILD_ID.parse().unwrap();
            let (raid, mut raid_supervisor) = raid_runtime::start_from_env(
                pool.clone(),
                executor.clone(),
                guild,
                stopping.clone(),
            );
            let raid_observer: Arc<dyn JoinObserver> = raid.clone();
            let (observer, risk_supervisor) =
                join_risk_runtime::chain_from_env(pool, executor, guild, raid.clone(), stopping);
            let mut risk_supervisor = risk_supervisor.expect("enabled risk worker is supervised");
            assert!(!Arc::ptr_eq(&observer, &raid_observer), "install fanout");
            assert!(futures_util::poll!(&mut raid_supervisor).is_pending());
            assert!(futures_util::poll!(&mut risk_supervisor).is_pending());
            shutdown.send(true).expect("service shutdown");
            joined(risk_supervisor).await;
            joined(raid_supervisor).await;
            assert_eq!(raid.finished(), 0, "no DB-dependent observations queued");
            assert_eq!(raid.dropped_shutdown(), 0);
            drop((observer, raid_observer));
            assert_eq!(
                Arc::strong_count(&raid),
                1,
                "fanout and supervisor release raid"
            );
            support::assert_no_discord(&trap);
        },
    );
}
