use std::sync::Mutex as StdMutex;

use serde_json::json;
use tokio::sync::watch;
use two_bot_core::settings::{SettingRow, SettingsSnapshot};
use two_bot_testsupport::TestDatabase;

use super::*;

fn row(guild: &str, key: &str, value: serde_json::Value) -> SettingRow {
    SettingRow {
        guild_id: guild.to_owned(),
        key: key.to_owned(),
        value,
        version: 1,
    }
}

/// Capture every formatted tracing event on this thread while the guard
/// lives. `set_default` is thread-scoped; `#[tokio::test]` is current-thread,
/// so supervised job tasks land in the buffer too.
#[derive(Clone, Default)]
struct Capture(Arc<StdMutex<String>>);

impl Capture {
    fn contents(&self) -> String {
        self.0.lock().expect("capture lock").clone()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn captured() -> (Capture, tracing::subscriber::DefaultGuard) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

#[test]
fn job_specification_matches_the_supervised_contract() {
    let job = job("postgres://unused-not-connected-until-first-tick");
    assert_eq!(job.name, "settings");
    assert_eq!(job.cadence, Duration::from_secs(POLL_SECONDS));
    assert_eq!(job.timeout, TIMEOUT);
    assert!(job.startup_jitter <= job.cadence.min(Duration::from_secs(5)));
    let reader = live().expect("registration exposes the feature reader");
    assert_eq!(reader.revision(), 0);
}

#[test]
fn snapshot_validation_refuses_only_malformed_rows() {
    let valid = SettingsSnapshot {
        revision: 3,
        rows: vec![
            row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3)),
            row("g2", "TWO_RAID_JOIN_THRESHOLD", json!(9)),
        ],
    };
    assert!(validate_snapshot(&valid).is_ok());
    for (guild, key) in [("", "K"), ("g1", "")] {
        let bad = SettingsSnapshot {
            revision: 4,
            rows: vec![row(guild, key, json!(1))],
        };
        assert_eq!(
            validate_snapshot(&bad),
            Err("empty guild_id or key"),
            "{guild}/{key}"
        );
    }
    let duplicate = SettingsSnapshot {
        revision: 4,
        rows: vec![row("g1", "K", json!(1)), row("g1", "K", json!(2))],
    };
    assert_eq!(
        validate_snapshot(&duplicate),
        Err("duplicate guild_id/key pair")
    );
}

#[test]
fn applied_swap_logs_spec_events_and_never_cold_values() {
    let (mut writer, _reader) = live_channel();
    let report = writer.publish(&SettingsSnapshot {
        revision: 7,
        rows: vec![
            row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(5)),
            row("g1", "TWO_FEED_POLL_SECONDS", json!(600)),
            row("g1", "TWO_MADE_UP_KEY", json!("unlisted")),
        ],
    });
    let (capture, _guard) = captured();
    log_applied(&report);
    let log = capture.contents();
    for needle in [
        "settings_applied",
        "setting_changed",
        "settings_restart_required",
        "setting_ignored_not_applied",
        "TWO_RAID_JOIN_THRESHOLD",
        "TWO_FEED_POLL_SECONDS",
        "TWO_MADE_UP_KEY",
    ] {
        assert!(log.contains(needle), "missing {needle}: {log}");
    }
    let applied = log
        .lines()
        .find(|line| line.contains("settings_applied"))
        .unwrap();
    assert!(applied.contains("TWO_RAID_JOIN_THRESHOLD"), "{applied}");
    assert!(
        !applied.contains("TWO_FEED_POLL_SECONDS"),
        "cold key reported as applied: {applied}"
    );
    // Key names only for the applied line's `keys`; cold values are never rendered.
    assert!(!log.contains("600"), "cold value leaked: {log}");
    assert!(!log.contains("unlisted"), "ignored value leaked: {log}");
}

#[test]
fn unchanged_report_logs_nothing() {
    let (mut writer, _reader) = live_channel();
    let snapshot = SettingsSnapshot {
        revision: 1,
        rows: vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(5))],
    };
    writer.publish(&snapshot);
    let report = writer.publish(&snapshot);
    let (capture, _guard) = captured();
    log_applied(&report);
    assert!(
        capture.contents().is_empty(),
        "no-change publish must not log: {}",
        capture.contents()
    );
}

/// The real acceptance path: a supervised poll against a migrated
/// agent-testdb fixture, driven by `SettingsStore::set` exactly like the
/// admin writer, plus one hand-inserted malformed row to prove the previous
/// snapshot survives a rejected poll.
#[tokio::test]
async fn supervised_poll_applies_real_guild_settings_writes() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP settings job integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();

    let (poll, reader) = Poll::with_pool(pool.clone());
    let poll = Arc::new(poll);
    let status = jobs::statuses(&[NAME], false);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(jobs::supervise(
        vec![Job {
            name: NAME,
            cadence: Duration::from_millis(40),
            startup_jitter: Duration::ZERO,
            timeout: Duration::from_secs(5),
            action: Arc::new(move || {
                let poll = Arc::clone(&poll);
                Box::pin(async move { poll.tick().await })
            }),
        }],
        status.clone(),
        rx,
    ));

    async fn until(
        status: &jobs::SharedStatus,
        what: &str,
        ready: impl Fn(&crate::jobs::JobStatus) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if ready(&status.read().await[NAME]) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    async fn until_change(what: &str, ready: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    // First ticks succeed against the seeded empty table; readers see the
    // revision-0 empty cache (env fallback) until a write lands.
    until(&status, "initial poll success", |s| {
        s.last_success.is_some()
    })
    .await;
    assert_eq!(reader.get("g1", "TWO_RAID_JOIN_THRESHOLD"), None);

    // A real store write must be visible through the typed read API within
    // one tick.
    let store = SettingsStore::new(&pool);
    store
        .set("g1", "TWO_RAID_JOIN_THRESHOLD", Some(json!(5)), "test")
        .await
        .expect("hot write");
    until_change("hot value publish", || {
        reader.get("g1", "TWO_RAID_JOIN_THRESHOLD") == Some(json!(5))
    })
    .await;
    let published_rev = reader.revision();
    assert!(published_rev >= 1);

    // An unchanged version must not republish: same Arc across several ticks.
    let before = reader.snapshot();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(Arc::ptr_eq(&before, &reader.snapshot()));

    // A malformed row (empty key — the PK/NOT NULL/CHECKs all permit it) must
    // fail the tick as Configuration and leave the previous snapshot live.
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
         VALUES ('g1', '', '{}'::jsonb, 1, 'test')",
    )
    .execute(&pool)
    .await
    .expect("insert malformed row");
    until(&status, "configuration failure", |s| {
        s.last_error_class == Some(ErrorClass::Configuration)
    })
    .await;
    assert_eq!(reader.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(json!(5)));
    assert_eq!(reader.revision(), published_rev);
    assert!(Arc::ptr_eq(&before, &reader.snapshot()));

    // Removing the malformed row recovers on its own.
    sqlx::query("DELETE FROM guild_settings WHERE key = ''")
        .execute(&pool)
        .await
        .expect("delete malformed row");
    until(&status, "recovery after malformed row", |s| {
        s.last_error_class.is_none() && s.last_success.is_some()
    })
    .await;
    assert_eq!(reader.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(json!(5)));

    // A cold key is stored and logged as restart-required, but never exposed
    // through any live read API, including after a subsequent update.
    let (capture, _guard) = captured();
    for value in [600, 900] {
        store
            .set("g1", "TWO_FEED_POLL_SECONDS", Some(json!(value)), "test")
            .await
            .expect("cold write");
        let (revision, _) = store.poll_marks().await.expect("cold write revision");
        until_change("cold revision observed", || reader.revision() == revision).await;
        assert_eq!(reader.get("g1", "TWO_FEED_POLL_SECONDS"), None);
        assert_eq!(reader.snapshot().get("g1", "TWO_FEED_POLL_SECONDS"), None);
        assert!(!reader
            .env_snapshot(Some("g1"))
            .contains_key("TWO_FEED_POLL_SECONDS"));
        let stored: serde_json::Value = sqlx::query_scalar(
            "SELECT value FROM guild_settings WHERE guild_id = 'g1' AND key = 'TWO_FEED_POLL_SECONDS'",
        )
        .fetch_one(&pool)
        .await
        .expect("cold row remains stored");
        assert_eq!(stored, json!(value));
    }
    until_change("restart-required log", || {
        capture.contents().contains("settings_restart_required")
    })
    .await;
    let log = capture.contents();
    assert!(log.contains("settings_applied"), "{log}");
    assert!(log.contains("TWO_FEED_POLL_SECONDS"), "{log}");
    assert!(!log.contains("600"), "cold value leaked: {log}");
    assert!(!log.contains("900"), "updated cold value leaked: {log}");

    stop.send(true).unwrap();
    task.await.unwrap();
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}
