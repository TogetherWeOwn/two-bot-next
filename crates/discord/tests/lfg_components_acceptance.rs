//! Acceptance tests: LFG post/recover renders degrade when the rendered
//! payload's `components` is missing or not an array.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! - A new associated function on `LfgInteractions` in
//!   `crates/discord/src/lfg_interactions.rs`, visible to integration
//!   tests:
//!   `pub fn payload_components(payload: &serde_json::Value) -> &[serde_json::Value]`
//! - It returns the payload's `components` array unchanged when present.
//! - A missing, null, or non-array `components` degrades to an empty slice
//!   and emits an `ERROR`-level log naming the degraded `components`
//!   field; the send proceeds with the empty list.
//! - Both render call sites (post and recover/refresh) route through this
//!   function; no `expect("rendered array")` remains on the LFG send path.
//! - Normal payloads are byte-identical: the returned slice equals the
//!   input array element for element, and no error is logged.
//!
//! Offline only: no database, no network, no Discord traffic. These tests
//! run in the ordinary workspace sweep with no per-test opt-out.
#![cfg(feature = "db")]

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use two_bot_discord::lfg_interactions::LfgInteractions;

/// Pre-fix behavior of the LFG render call sites: index `components` and
/// panic when it is not an array. On the baseline these tests resolve
/// `LfgInteractions::payload_components` to this trait method and fail;
/// once the builder adds the inherent function of the same name, inherent
/// items win and the same calls exercise the fix.
trait BaselinePayloadComponents {
    fn payload_components(payload: &Value) -> &[Value];
}

impl BaselinePayloadComponents for LfgInteractions {
    fn payload_components(payload: &Value) -> &[Value] {
        payload["components"].as_array().expect("rendered array")
    }
}

// One process-wide capture: a scoped subscriber drops events under parallel
// tests, so the log-reading tests serialize on LOG_GUARD instead.
static LOG_GUARD: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn capture() -> Self {
        static LOG: OnceLock<Log> = OnceLock::new();
        let log = LOG
            .get_or_init(|| {
                let log = Log::default();
                let writer = log.clone();
                tracing::subscriber::set_global_default(
                    tracing_subscriber::fmt()
                        .without_time()
                        .with_ansi(false)
                        .with_writer(move || writer.clone())
                        .finish(),
                )
                .expect("install log capture");
                log
            })
            .clone();
        log.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        log
    }

    fn text(&self) -> String {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn lock_logs() -> std::sync::MutexGuard<'static, ()> {
    LOG_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn missing_components_degrades_to_empty_slice_with_error_log() {
    let _guard = lock_logs();
    let log = Log::capture();
    let payload = json!({"content": "Friday raid", "allowed_mentions": {"parse": []}});
    let components = LfgInteractions::payload_components(&payload);
    assert!(
        components.is_empty(),
        "missing components must degrade to an empty list"
    );
    let text = log.text();
    assert!(
        text.contains("ERROR"),
        "degrading a missing components list must log an error, got: {text}"
    );
    assert!(
        text.contains("components"),
        "the degrade error must name the components field, got: {text}"
    );
}

#[test]
fn non_array_components_degrades_to_empty_slice_with_error_log() {
    let _guard = lock_logs();
    for components in [
        Value::Null,
        json!(false),
        json!(42),
        json!("picked"),
        json!({"type": 1}),
    ] {
        let log = Log::capture();
        let payload = json!({"content": "Friday raid", "components": components});
        let degraded = LfgInteractions::payload_components(&payload);
        assert!(
            degraded.is_empty(),
            "non-array components must degrade to an empty list"
        );
        let text = log.text();
        assert!(
            text.contains("ERROR"),
            "degrading a non-array components list must log an error, got: {text}"
        );
        assert!(
            text.contains("components"),
            "the degrade error must name the components field, got: {text}"
        );
    }
}

#[test]
fn array_components_pass_through_byte_identical() {
    let _guard = lock_logs();
    let log = Log::capture();
    let row = json!({"type": 1, "components": [{"type": 3, "custom_id": "lfg:post", "options": [{"label": "Tank", "value": "tank"}]}]});
    let payload = json!({"content": "Friday raid", "components": [row.clone()]});
    let passed = LfgInteractions::payload_components(&payload);
    assert_eq!(
        passed,
        &[row],
        "a present components array must pass through unchanged"
    );
    assert!(
        !log.text().contains("ERROR"),
        "a normal render must not log an error, got: {}",
        log.text()
    );
}

#[test]
fn empty_array_passes_through_without_error_log() {
    let _guard = lock_logs();
    let log = Log::capture();
    let payload = json!({"content": "Friday raid", "components": []});
    let passed = LfgInteractions::payload_components(&payload);
    assert!(passed.is_empty(), "an empty array stays empty");
    assert!(
        !log.text().contains("ERROR"),
        "an empty array is normal, not a degrade, got: {}",
        log.text()
    );
}

#[test]
#[should_panic(expected = "rendered array")]
fn baseline_adapter_still_panics_on_missing_components() {
    // Legacy-compat control: keeps `BaselinePayloadComponents` live after
    // the builder's inherent function shadows it (clippy `-D warnings`
    // fails the build on a dead trait the builder may not remove).
    let payload = json!({"content": "Friday raid"});
    let _ = <LfgInteractions as BaselinePayloadComponents>::payload_components(&payload);
}
