#![cfg(test)]
//! Offline snapshots and structured tracing capture shared by tuning tests.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Layer;
use two_bot_core::settings::{SettingRow, SettingsCache, SettingsSnapshot};

pub const GUILD: u64 = 123456789012345678;
pub const SENTINEL: &str = "private-invalid-tuning-sentinel";

/// Tracing callsite interest is process-global. Run log assertions alone so
/// parallel tests installing dispatchers cannot hide or contaminate events.
pub fn isolated_log_test() -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("named test thread");
    if std::env::var("TWO_TUNING_LOG_TEST_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", name, "--nocapture"])
        .env("TWO_TUNING_LOG_TEST_CHILD", name)
        .output()
        .expect("isolated log test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "test must actually run: {stdout}"
    );
    true
}

pub fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

pub fn snapshot(pairs: &[(&str, &str)]) -> SettingsCache {
    SettingsCache::load(&SettingsSnapshot {
        revision: 1,
        rows: pairs
            .iter()
            .map(|(key, value)| SettingRow {
                guild_id: GUILD.to_string(),
                key: (*key).to_owned(),
                value: serde_json::json!(value),
                version: 1,
            })
            .collect(),
    })
}

/// Close before use: no network connection is possible, including a refresh.
pub async fn closed_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_tuning")
        .expect("synthetic lazy pool");
    pool.close().await;
    pool
}

/// A listening but non-serving loopback socket: any attempted REST connection
/// is observable, and no request can ever leave this process's host.
pub fn no_discord() -> (two_bot_discord::ActionExecutor, std::net::TcpListener) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback trap");
    listener.set_nonblocking(true).expect("nonblocking trap");
    let origin = format!("http://{}", listener.local_addr().expect("trap address"));
    let executor =
        two_bot_discord::ActionExecutor::with_proxy("tuning-test-only-token".into(), Some(origin))
            .expect("offline executor");
    (executor, listener)
}

pub fn assert_no_discord(listener: &std::net::TcpListener) {
    let error = listener
        .accept()
        .expect_err("no Discord connection may be attempted");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}

#[derive(Clone, Debug)]
pub struct Record {
    pub level: Level,
    pub fields: HashMap<String, String>,
}

impl Visit for Record {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<Record>>>);

impl Capture {
    pub fn subscriber(&self) -> impl Subscriber + Send + Sync + 'static {
        tracing_subscriber::registry().with(self.clone())
    }

    pub fn records(&self) -> Vec<Record> {
        self.0.lock().expect("capture lock").clone()
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut record = Record {
            level: *event.metadata().level(),
            fields: HashMap::new(),
        };
        event.record(&mut record);
        self.0.lock().expect("capture lock").push(record);
    }
}
