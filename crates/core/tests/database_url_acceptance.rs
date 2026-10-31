#![cfg(feature = "db")]

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgConnectOptions;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;
use two_bot_core::database_url::{connect_options, validate};

// Count events, never visit their fields: even the negative control retains no
// URL, key, value, message text, or remote-controlled label.
struct EventCount(Arc<AtomicUsize>);

impl<S: Subscriber> Layer<S> for EventCount {
    fn on_event(&self, _: &tracing::Event<'_>, _: Context<'_, S>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn count_events<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let count = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(EventCount(count.clone()));
    let result = tracing::subscriber::with_default(subscriber, run);
    (result, count.load(Ordering::Relaxed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Invalid,
    Unsupported,
    Unexpected,
}

fn validation_outcome(result: Result<(), &'static str>) -> Outcome {
    match result {
        Ok(()) => Outcome::Accepted,
        Err("invalid database URL") => Outcome::Invalid,
        Err("unsupported database URL parameter") => Outcome::Unsupported,
        Err(_) => Outcome::Unexpected,
    }
}

fn connection_outcome(raw: &str) -> Outcome {
    match connect_options(raw) {
        Ok(_) => Outcome::Accepted,
        Err(sqlx::Error::InvalidArgument(message)) => match message.as_str() {
            "invalid database URL" => Outcome::Invalid,
            "unsupported database URL parameter" => Outcome::Unsupported,
            _ => Outcome::Unexpected,
        },
        Err(_) => Outcome::Unexpected,
    }
}

fn assert_silent(raw: &str, expected_validation: Outcome, expected_connection: Outcome) {
    let ((validated, connected), events) =
        count_events(|| (validation_outcome(validate(raw)), connection_outcome(raw)));
    assert_eq!(validated, expected_validation);
    assert_eq!(connected, expected_connection);
    assert_eq!(events, 0, "database URL parse emitted a diagnostic");
}

#[test]
fn event_counter_detects_the_raw_driver_negative_control() {
    // Explicit synthetic credentials avoid ambient passfile lookup. FromStr
    // parses only; it never connects to this reserved .invalid host.
    let (_, events) = count_events(|| {
        PgConnectOptions::from_str(
            "postgres://fixture:fixture-password@database.invalid/db?unknown=fixture-value",
        )
        .is_ok()
    });
    assert!(
        events > 0,
        "negative control must observe the driver's WARN"
    );
}

#[test]
fn unsupported_queries_refuse_silently_even_when_driver_values_are_invalid() {
    for key in [
        "api_key",
        "api%5Fkey",
        "api%255Fkey",
        "sslMode",
        "sslmode%00",
        "sslmode+",
        "options[unfinished",
        "options[closed]suffix",
        "%0d%0a",
        "%FF",
        "\u{200b}",
        "",
    ] {
        for suffix in [
            "sslmode=require",
            "sslmode=fixture-invalid",
            "port=fixture-invalid",
        ] {
            let raw = format!(
                "postgres://fixture:fixture-password@database.invalid/db?{suffix}&{key}=fixture-value"
            );
            assert_silent(&raw, Outcome::Unsupported, Outcome::Unsupported);
        }
    }
    let repeated = format!(
        "postgres://fixture:fixture-password@database.invalid/db?{}",
        "channel_binding=fixture-value&api_key=fixture-value&".repeat(1024)
    );
    assert_silent(&repeated, Outcome::Unsupported, Outcome::Unsupported);
}

#[test]
fn supported_invalid_values_have_only_the_fixed_driver_refusal() {
    for query in [
        "sslmode=fixture-invalid",
        "ssl-mode=fixture-invalid",
        "port=fixture-invalid",
        "port=65536",
        "statement-cache-capacity=fixture-invalid",
        "sslmode=require%00fixture-value",
    ] {
        let raw = format!("postgres://fixture:fixture-password@database.invalid/db?{query}");
        assert_silent(&raw, Outcome::Accepted, Outcome::Invalid);
    }
    for raw in [
        "",
        "not a database url",
        "postgres://[::1/db",
        "\0",
        "\u{1f600}",
    ] {
        assert_silent(raw, Outcome::Invalid, Outcome::Invalid);
    }
}

#[test]
fn committed_fuzz_seeds_replay_with_exact_outcomes_and_no_diagnostics() {
    for (raw, validated, connected) in [
        (
            include_str!("../../../fuzz/corpus/database_url/seed-valid"),
            Outcome::Accepted,
            Outcome::Accepted,
        ),
        (
            include_str!("../../../fuzz/corpus/database_url/seed-channel-binding"),
            Outcome::Accepted,
            Outcome::Accepted,
        ),
        (
            include_str!("../../../fuzz/corpus/database_url/seed-unsupported-secret"),
            Outcome::Unsupported,
            Outcome::Unsupported,
        ),
        (
            include_str!("../../../fuzz/corpus/database_url/seed-invalid"),
            Outcome::Invalid,
            Outcome::Invalid,
        ),
        (
            include_str!("../../../fuzz/corpus/database_url/seed-hostile-encoded"),
            Outcome::Unsupported,
            Outcome::Unsupported,
        ),
        (
            include_str!("../../../fuzz/corpus/database_url/seed-bad-value"),
            Outcome::Accepted,
            Outcome::Invalid,
        ),
    ] {
        assert_silent(raw, validated, connected);
    }
}

#[test]
fn campaign_sized_values_preserve_silent_refusal_and_valid_compatibility() {
    let value = "x".repeat(65_536);
    let unsupported =
        format!("postgres://fixture:fixture-password@database.invalid/db?api_key={value}");
    let supported =
        format!("postgres://fixture:fixture-password@database.invalid/db?sslmode={value}");
    for _ in 0..3 {
        assert_silent(&unsupported, Outcome::Unsupported, Outcome::Unsupported);
        assert_silent(&supported, Outcome::Accepted, Outcome::Invalid);
        assert_silent(
            "postgres://fixture:fixture-password@database.invalid/db?channel%5Fbinding=fixture-value&sslmode=require&channel_binding=require",
            Outcome::Accepted,
            Outcome::Accepted,
        );
    }
}
