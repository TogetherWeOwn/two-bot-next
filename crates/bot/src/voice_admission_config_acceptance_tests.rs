//! Acceptance tests: temp-voice admission caps and cooldown follow the
//! configured settings instead of the hardcoded `(1, 40, 0)`.
//!
//! The three cold-class keys `TWO_TEMP_VOICE_MAX_PER_USER`,
//! `TWO_TEMP_VOICE_MAX_PER_GUILD` and
//! `TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS` are read once at boot and must
//! reach every guild worker's durable create admission. Unset keys keep
//! today's effective behavior exactly — 1 room per member, 40 per guild,
//! and **no** cooldown (0 seconds; deliberately not the legacy core
//! default of 30 s, which silently kept members who rejoined the creator
//! in the lobby). A present-but-unparsable or out-of-range value fails
//! closed to that knob's default only, with a warning naming the key and
//! the reason `invalid_value`; the other knobs are still honored, and no
//! value can panic the loader.
//!
//! Builder contract (this file is preserved verbatim; see the manifest
//! on the acceptance card):
//!
//! - `pub fn configured_admission(lookup: impl Fn(&str) -> Option<String>)
//!   -> CreateAdmissionConfig` in `voice_rooms`, shaped like
//!   `configured_protected_channels`. It looks up exactly the three keys
//!   above, trims values, treats missing/empty as unset, validates each
//!   knob against the core ranges (per-user 1..=10, per-guild 1..=45,
//!   cooldown 0..=3600) through `CreateAdmissionConfig` semantics, and
//!   warns once per unusable value with the key and reason
//!   `invalid_value` (catalog the new message for the observability
//!   conformance tests).
//! - `VoiceRuntime::with_admission_config(config)` stores the boot config
//!   in an `admission_config: Option<CreateAdmissionConfig>` field, and
//!   actor spawn applies it via `GuildRoomWorker::set_admission_config`
//!   before the actor runs (mirroring the empty-grace boot wiring). No
//!   `with_admission_config` call keeps the worker's current
//!   `temp_voice_admission()` default untouched.
//! - `build_production_runtime` feeds the runtime from the environment:
//!   `configured_admission(|key| std::env::var(key).ok())`.
//! - `docs/configuration.md` no longer marks the three keys stored
//!   unwired.
//!
//! No hot reload, no new setting key, no burst-window or refusal-copy
//! change. The `control_` test uses only today's seams and passes on
//! current main once this file compiles; it must stay green.

use super::*;

const MAX_PER_USER_KEY: &str = "TWO_TEMP_VOICE_MAX_PER_USER";
const MAX_PER_GUILD_KEY: &str = "TWO_TEMP_VOICE_MAX_PER_GUILD";
const COOLDOWN_KEY: &str = "TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS";

/// Today's effective admission: (max_per_user, max_per_guild, cooldown).
const TODAY: (u32, u32, u32) = (1, 40, 0);

fn knobs(config: CreateAdmissionConfig) -> (u32, u32, u32) {
    (
        config.max_per_user(),
        config.max_per_guild(),
        config.cooldown_secs(),
    )
}

fn admission_from(pairs: &[(&str, &str)]) -> CreateAdmissionConfig {
    let values: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    configured_admission(|key| values.get(key).cloned())
}

#[test]
fn loader_reads_exactly_the_three_admission_keys() {
    let asked = std::cell::RefCell::new(Vec::new());
    let config = configured_admission(|key| {
        asked.borrow_mut().push(key.to_owned());
        None
    });
    assert_eq!(knobs(config), TODAY);
    let mut asked = asked.into_inner();
    asked.sort();
    assert_eq!(
        asked,
        vec![
            COOLDOWN_KEY.to_owned(),
            MAX_PER_GUILD_KEY.to_owned(),
            MAX_PER_USER_KEY.to_owned(),
        ],
        "the loader must consult exactly the three catalogued keys, once each"
    );
}

#[test]
fn unset_keys_keep_todays_effective_config() {
    let config = admission_from(&[]);
    assert_eq!(knobs(config), TODAY);
    // Pin the deliberate divergence from the legacy core default: wiring
    // `CreateAdmissionConfig::default()` in would resurrect the 30 s
    // cooldown today's runtime does not have.
    assert_eq!(CreateAdmissionConfig::default().cooldown_secs(), 30);
    assert_ne!(
        knobs(config),
        knobs(CreateAdmissionConfig::default()),
        "unset must keep today's (1, 40, 0), not the legacy (1, 40, 30)"
    );
}

#[test]
fn empty_and_whitespace_values_count_as_unset() {
    for blank in ["", " ", "  \t "] {
        let config = admission_from(&[
            (MAX_PER_USER_KEY, blank),
            (MAX_PER_GUILD_KEY, blank),
            (COOLDOWN_KEY, blank),
        ]);
        assert_eq!(knobs(config), TODAY, "{blank:?} must count as unset");
    }
}

#[test]
fn configured_values_reach_the_config() {
    let config = admission_from(&[
        (MAX_PER_USER_KEY, "3"),
        (MAX_PER_GUILD_KEY, "25"),
        (COOLDOWN_KEY, "90"),
    ]);
    assert_eq!(knobs(config), (3, 25, 90));
    // Surrounding whitespace is not part of the value.
    let config = admission_from(&[(MAX_PER_USER_KEY, " 4 ")]);
    assert_eq!(config.max_per_user(), 4);
}

#[test]
fn boundary_values_are_accepted() {
    for (value, expected) in [("1", 1), ("10", 10)] {
        let config = admission_from(&[(MAX_PER_USER_KEY, value)]);
        assert_eq!(config.max_per_user(), expected, "per-user {value}");
    }
    for (value, expected) in [("1", 1), ("45", 45)] {
        let config = admission_from(&[(MAX_PER_GUILD_KEY, value)]);
        assert_eq!(config.max_per_guild(), expected, "per-guild {value}");
    }
    for (value, expected) in [("0", 0), ("3600", 3600)] {
        let config = admission_from(&[(COOLDOWN_KEY, value)]);
        assert_eq!(config.cooldown_secs(), expected, "cooldown {value}");
    }
}

/// One bad knob must not poison the others: the two valid, non-default
/// companions survive while the bad knob alone falls back to its default.
#[test]
fn out_of_range_values_fail_closed_per_knob() {
    for bad in ["0", "11"] {
        let config = admission_from(&[
            (MAX_PER_USER_KEY, bad),
            (MAX_PER_GUILD_KEY, "25"),
            (COOLDOWN_KEY, "90"),
        ]);
        assert_eq!(knobs(config), (1, 25, 90), "per-user {bad}");
    }
    for bad in ["0", "46"] {
        let config = admission_from(&[
            (MAX_PER_USER_KEY, "3"),
            (MAX_PER_GUILD_KEY, bad),
            (COOLDOWN_KEY, "90"),
        ]);
        assert_eq!(knobs(config), (3, 40, 90), "per-guild {bad}");
    }
    let config = admission_from(&[
        (MAX_PER_USER_KEY, "3"),
        (MAX_PER_GUILD_KEY, "25"),
        (COOLDOWN_KEY, "3601"),
    ]);
    assert_eq!(knobs(config), (3, 25, 0), "cooldown 3601");
}

#[test]
fn unparsable_values_fail_closed_per_knob() {
    for bad in ["abc", "-1", "1.5", "99999999999999999999999999"] {
        let config = admission_from(&[
            (MAX_PER_USER_KEY, bad),
            (MAX_PER_GUILD_KEY, "25"),
            (COOLDOWN_KEY, "90"),
        ]);
        assert_eq!(knobs(config), (1, 25, 90), "per-user {bad:?}");
        let config = admission_from(&[
            (MAX_PER_USER_KEY, "3"),
            (MAX_PER_GUILD_KEY, bad),
            (COOLDOWN_KEY, "90"),
        ]);
        assert_eq!(knobs(config), (3, 40, 90), "per-guild {bad:?}");
        let config = admission_from(&[
            (MAX_PER_USER_KEY, "3"),
            (MAX_PER_GUILD_KEY, "25"),
            (COOLDOWN_KEY, bad),
        ]);
        assert_eq!(knobs(config), (3, 25, 0), "cooldown {bad:?}");
    }
}

#[test]
fn all_invalid_values_yield_todays_config_without_panic() {
    let config = admission_from(&[
        (MAX_PER_USER_KEY, "0"),
        (MAX_PER_GUILD_KEY, "many"),
        (COOLDOWN_KEY, "-30"),
    ]);
    assert_eq!(knobs(config), TODAY);
}

/// An unusable value leaves one warning naming its key with the reason
/// `invalid_value`, so an operator can find why the default is in force.
/// Unset and valid values log no such reason.
#[test]
fn invalid_values_log_a_named_reason() {
    super::tracing_capture::global();
    for (key, bad) in [
        (MAX_PER_USER_KEY, "11"),
        (MAX_PER_GUILD_KEY, "0"),
        (COOLDOWN_KEY, "3601"),
        (MAX_PER_USER_KEY, "abc"),
    ] {
        let capture = super::tracing_capture::Capture::default();
        let config =
            tracing::subscriber::with_default(capture.clone(), || admission_from(&[(key, bad)]));
        let logged = capture.text();
        assert!(
            logged
                .lines()
                .any(|line| line.contains(key) && line.contains("invalid_value")),
            "expected one warning naming {key} with reason invalid_value for {bad:?}: {logged}"
        );
        assert_eq!(knobs(config), TODAY, "{key}={bad:?} must fail closed");
    }

    let capture = super::tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        admission_from(&[]);
        admission_from(&[
            (MAX_PER_USER_KEY, "3"),
            (MAX_PER_GUILD_KEY, "25"),
            (COOLDOWN_KEY, "90"),
        ]);
    });
    let logged = capture.text();
    assert!(
        !logged.contains("invalid_value"),
        "unset and valid values log no invalid_value reason: {logged}"
    );
}

/// Control: a worker loaded with no configured admission keeps today's
/// effective config. Uses only today's seams; must stay green.
#[tokio::test]
async fn control_worker_default_effective_config_is_todays() {
    let (live, store, http, _trace) = fixture();
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert_eq!(knobs(worker.admission), TODAY);
}

/// Worker effective config: what the loader resolves from the configured
/// values is exactly what the worker enforces — visible both in the
/// worker's admission config and in the refusal copy rendered from it
/// (no Discord call on a refused create).
#[tokio::test]
async fn worker_effective_config_follows_the_loader() {
    let configured = admission_from(&[
        (MAX_PER_USER_KEY, "3"),
        (MAX_PER_GUILD_KEY, "25"),
        (COOLDOWN_KEY, "45"),
    ]);

    let (live, store, http, _trace) = fixture();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.set_admission_config(configured);
    assert_eq!(knobs(worker.admission), (3, 25, 45));

    for (reason, expected) in [
        (
            RefusalReason::UserCap,
            "You already have 3 temporary voice channels.",
        ),
        (
            RefusalReason::Cooldown,
            "Please wait 45 seconds between creating channels.",
        ),
    ] {
        let message = refused_message(reason, configured).await;
        assert_eq!(message, expected, "{reason:?} refusal copy");
    }
}

async fn refused_message(reason: RefusalReason, config: CreateAdmissionConfig) -> String {
    let (live, store, http, trace) = fixture();
    store
        .claim_script
        .lock()
        .unwrap()
        .push_back(Ok(CreateClaim::Refused(reason)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.set_admission_config(config);
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert!(
        trace.lock().unwrap().is_empty(),
        "{reason:?}: a refused create makes no Discord call"
    );
    match worker.failures().back() {
        Some(LifecycleFailure::CreateRefused { message, .. }) => message.clone(),
        other => panic!("expected a create refusal, got {other:?}"),
    }
}

/// The runtime carries the boot-resolved config to its actors; without
/// one installed it carries nothing and workers keep today's default.
#[test]
fn runtime_carries_the_boot_admission_config() {
    let configured = admission_from(&[
        (MAX_PER_USER_KEY, "2"),
        (MAX_PER_GUILD_KEY, "30"),
        (COOLDOWN_KEY, "15"),
    ]);
    let runtime: VoiceRuntime<Store, Http> = VoiceRuntime::new(
        || (Store::new(Trace::default()), Http::new(Trace::default())),
        Duration::from_millis(10),
        true,
    )
    .with_admission_config(configured);
    assert_eq!(runtime.admission_config, Some(configured));

    let bare: VoiceRuntime<Store, Http> = VoiceRuntime::new(
        || (Store::new(Trace::default()), Http::new(Trace::default())),
        Duration::from_millis(10),
        true,
    );
    assert_eq!(bare.admission_config, None);
}

/// Wiring pin: the loader must actually be wired into production — this
/// is the regression this whole slice exists to close (`set_admission_
/// config` shipped with no production caller). The production runtime
/// resolves the environment through the loader, a production call site
/// applies the result to workers, and the operator docs stop calling
/// the keys stored unwired.
#[test]
fn production_runtime_wires_the_loader_into_workers() {
    let source =
        std::fs::read_to_string(format!("{}/src/voice_rooms.rs", env!("CARGO_MANIFEST_DIR")))
            .expect("voice_rooms.rs is readable from the crate root");
    assert!(
        source.contains("configured_admission(|key| std::env::var(key).ok())"),
        "build_production_runtime must resolve the boot environment through configured_admission"
    );
    assert!(
        source.contains(".with_admission_config("),
        "the production runtime must install the resolved admission config"
    );
    assert!(
        source.matches("set_admission_config(").count() >= 2,
        "actor spawn must apply the runtime config via set_admission_config \
         (definition plus at least one production call site)"
    );

    let docs = std::fs::read_to_string(format!(
        "{}/../../docs/configuration.md",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("docs/configuration.md is readable from the crate root");
    for key in [MAX_PER_USER_KEY, MAX_PER_GUILD_KEY, COOLDOWN_KEY] {
        let row = docs
            .lines()
            .find(|line| line.contains(key))
            .unwrap_or_else(|| panic!("docs/configuration.md must keep a row for {key}"));
        assert!(
            row.contains("env at boot"),
            "{key} is wired now; its docs row must say so: {row}"
        );
    }
}
