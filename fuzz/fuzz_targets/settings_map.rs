#![no_main]

//! Hostile-values fuzz target for guild/assistant settings-map parsing.
//!
//! Dashboard-supplied settings maps (including
//! `AssistantConfig::from_map`, called out in
//! `docs/command-wiring-security.md`) accept untrusted-shaped values. This
//! target drives every pure `from_map` settings parser in `two-bot-core`
//! with arbitrary maps: hostile strings, huge values, wrong types (as
//! strings and as JSON values where the seam uses them) and unknown keys.
//!
//! Properties (fail-closed, no panic/hang/unbounded allocation, no secret
//! rendering):
//! - No `from_env` (process environment), no network, no database, no real
//!   secret. All input derives from the fuzzer bytes; seeds are synthetic.
//! - Input over 64 KiB is bounded by the documented `-max_len=65536`
//!   campaign flag; decoding additionally caps entries at 32 so one
//!   iteration stays fast (no hang) and total allocation stays O(input).
//! - Invalid UTF-8 is discarded (same convention as the `prefix_trigger`
//!   target for `&str` parsers); everything else is kept verbatim,
//!   including control characters, whitespace, unicode and `=`/`\n`.
//! - Unknown keys must be ignored: the target re-parses the filtered
//!   known-key map and asserts identical outcomes (never half-applied).
//! - `Secret<T>` spot-checks use fuzz-derived values and assert the
//!   constant `[REDACTED]` rendering under every format flag, so
//!   credentials never render on fuzz-found inputs.

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};
use two_bot_core::{
    automod::{AutomodConfig, AutomodPolicy},
    disable_preflight::DisableGates,
    feature_commands::FeatureGates,
    internal_action_config::InternalActionConfig,
    internal_actions::{
        build_channel_keys, build_role_keys, check_setting_value_size, require_settings_key,
        valid_settings_key_shape, InternalFlags,
    },
    moderation::ModerationGates,
    onboarding::OnboardingGates,
    self_roles::SelfRoleGates,
    settings::{
        assert_storable_key, is_declared_env_only, is_env_only_key, is_storable_key, to_env_string,
    },
    voice_assistant::{AssistantConfig, MAX_ENDPOINT_CHARS},
    voice_rooms::VoiceGates,
    Secret,
};

/// Bound entries per iteration so 64 KiB of single-character lines cannot
/// turn one iteration into a minutes-long `HashMap` build.
const MAX_ENTRIES: usize = 32;

/// Every key any driven parser reads. Unknown keys (anything else, including
/// `DISCORD_*` dashboard names the core parsers never read) must be ignored;
/// the harness asserts full-map and filtered-map outcomes match.
const KNOWN_KEYS: &[&str] = &[
    "TWO_ASSISTANT_ENDPOINT",
    "TWO_ASSISTANT_MODEL",
    "TWO_AUTOMATIONS",
    "TWO_ANNOUNCEMENTS",
    "TWO_TEXT_COMMANDS",
    "TWO_FEED_POLL_SECONDS",
    "TWO_MODERATION",
    "TWO_OWEN_USER_ID",
    "TWO_MODERATION_PROTECTED_ROLE_IDS",
    "TWO_VOICE",
    "TWO_AUTOMOD",
    "TWO_AUTOMOD_ENFORCE",
    "TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS",
    "TWO_AUTOMOD_REPEAT_COUNT",
    "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS",
    "TWO_AUTOMOD_MENTION_LIMIT",
    "TWO_AUTOMOD_BYPASS_ROLE_IDS",
    "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS",
    "TWO_AUTOMOD_SANCTIONS",
    "TWO_AUTOMOD_BAD_WORDS",
    "TWO_AUTOMOD_ALLOWED_DOMAINS",
    "TWO_ONBOARDING_MODE",
    "TWO_ONBOARDING_DRY_RUN",
    "TWO_SELF_ROLE_PANELS",
    "TWO_SELF_ROLE_DRY_RUN",
    "TWO_INTERNAL_ACTIONS",
    "TWO_INTERNAL_BIND",
    "TWO_INTERNAL_KEYS",
    "TWO_INTERNAL_CALLERS",
    "TWO_INTERNAL_CHANNEL_KEYS",
    "TWO_INTERNAL_CONTAINER",
    "TWO_INTERNAL_ROLE_KEYS",
    "TWO_INTERNAL_ALLOW_ADD_MEMBER",
    "TWO_INTERNAL_ALLOW_EVENT_CANCEL",
    "TWO_INTERNAL_ALLOW_EVENT_READ",
    "TWO_INTERNAL_ALLOW_AUTOMATIONS",
    "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE",
    "TWO_INTERNAL_ALLOW_SETTINGS",
    "TWO_INTERNAL_ALLOW_MODERATION",
];

fn decode_map(data: &[u8]) -> Option<HashMap<String, String>> {
    let text = std::str::from_utf8(data).ok()?;
    let mut map = HashMap::new();
    for line in text.split('\n').take(MAX_ENTRIES) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            map.entry(String::new()).or_insert_with(String::new);
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((key, value)) => (key, value),
            None => (line, ""),
        };
        map.insert(key.to_owned(), value.to_owned());
    }
    Some(map)
}

fn is_snowflake_like(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

fn check_secret_redaction(sample: &str) {
    let secret = Secret::new(sample.to_owned());
    for rendered in [
        format!("{secret}"),
        format!("{secret:?}"),
        format!("{secret:#?}"),
        format!("{secret:>100}"),
        format!("{secret:.3}"),
    ] {
        assert_eq!(rendered, "[REDACTED]");
    }
    assert_eq!(secret.expose(), sample);
}

fn drive_assistant(vars: &HashMap<String, String>) {
    if let Some(config) = AssistantConfig::from_map(vars) {
        let endpoint = config.endpoint();
        assert!(!endpoint.is_empty(), "enabled assistant needs an endpoint");
        assert!(
            endpoint.chars().count() <= MAX_ENDPOINT_CHARS,
            "enabled endpoint must fit the bound, never truncate"
        );
        assert!(
            !endpoint.chars().any(char::is_control),
            "control characters must disable, not redirect"
        );
        let lower = endpoint.to_ascii_lowercase();
        assert!(
            lower.starts_with("http://") || lower.starts_with("https://"),
            "enabled endpoint must be http(s)"
        );
        let rest = if lower.starts_with("http://") {
            &endpoint[7..]
        } else {
            &endpoint[8..]
        };
        assert!(!rest.trim().is_empty(), "scheme alone must stay disabled");
        assert_eq!(config.model(), config.model().trim());
        check_secret_redaction(endpoint);
        check_secret_redaction(config.model());
    }
}

fn drive_guild_gates(vars: &HashMap<String, String>) {
    if let Ok(gates) = FeatureGates::from_map(vars) {
        assert!(
            (60..=86_400).contains(&gates.feed_poll_seconds),
            "parsed poll interval must stay in range"
        );
    }
    match ModerationGates::from_map(vars) {
        Ok(gates) => {
            if gates.enabled {
                assert!(
                    is_snowflake_like(&gates.owen_user_id),
                    "enabled moderation needs a snowflake owner"
                );
            }
            for id in &gates.protected_role_ids {
                assert!(is_snowflake_like(id), "protected roles must be snowflakes");
            }
        }
        Err(_) => {}
    }
    let voice = VoiceGates::from_map(vars);
    assert_eq!(
        voice.enabled,
        vars.get("TWO_VOICE").is_some_and(|v| v == "1"),
        "hostile voice values must stay disabled"
    );
    let disable = DisableGates::from_map(vars);
    assert_eq!(
        disable.moderation,
        vars.get("TWO_MODERATION").is_some_and(|v| v == "1")
    );
    assert_eq!(
        disable.automations,
        vars.get("TWO_AUTOMATIONS").is_some_and(|v| v == "1")
    );
    match OnboardingGates::from_map(vars) {
        Ok(gates) => {
            assert_eq!(
                gates.dry_run,
                vars.get("TWO_ONBOARDING_DRY_RUN").is_some_and(|v| v == "1")
            );
        }
        Err(_) => {}
    }
    if let Ok(config) = AutomodConfig::from_map(vars) {
        assert!((2..=20).contains(&config.policy.repeated_message_count));
        assert!((1..=3600).contains(&config.policy.repeated_message_window_seconds));
        assert!((1..=50).contains(&config.policy.mention_limit));
    }
    let _ = AutomodPolicy::name_policy_from_map(vars);
    if let Ok(gates) = SelfRoleGates::from_map(vars) {
        if vars
            .get("TWO_SELF_ROLE_PANELS")
            .is_none_or(|v| v.trim().is_empty())
        {
            assert!(gates.panels.is_empty(), "empty catalogue disables");
        }
    }
}

fn drive_internal(vars: &HashMap<String, String>) {
    let flags = InternalFlags::from_map(vars);
    assert!(flags.is_enabled("role.assign"));
    assert!(flags.is_enabled("announcement.post"));
    assert!(flags.is_enabled("event.upsert"));
    assert_eq!(
        flags.is_enabled("guild.add_member"),
        vars.get("TWO_INTERNAL_ALLOW_ADD_MEMBER")
            .is_some_and(|v| v == "1")
    );
    assert_eq!(
        flags.is_enabled("settings.get"),
        vars.get("TWO_INTERNAL_ALLOW_SETTINGS")
            .is_some_and(|v| v == "1")
    );
    assert_eq!(
        flags.is_enabled("settings.set"),
        vars.get("TWO_INTERNAL_ALLOW_SETTINGS")
            .is_some_and(|v| v == "1")
    );

    match InternalActionConfig::from_map(vars) {
        Ok(Some(config)) => {
            assert_ne!(config.listen_addr().port(), 0);
            assert!((1..=64).contains(&config.keys().len()));
            assert!(!config.channel_keys().is_empty());
            let debug = format!("{config:?}");
            assert!(debug.contains("key_count"));
            assert!(debug.contains("channel_count"));
        }
        Ok(None) => {}
        Err(_) => {}
    }

    let spec_key = vars
        .get("TWO_INTERNAL_ROLE_KEYS")
        .or(vars.get("TWO_INTERNAL_CHANNEL_KEYS"))
        .or(vars.values().next());
    if let Some(spec) = spec_key {
        let _ = build_role_keys(spec);
        let _ = build_channel_keys(spec);
    }
}

fn drive_settings_catalogue(vars: &HashMap<String, String>) {
    for key in vars.keys().take(8) {
        assert_eq!(is_storable_key(key), !is_env_only_key(key));
        if key.starts_with("TWO_INTERNAL_") {
            assert!(
                is_env_only_key(key),
                "the internal namespace is always env-only"
            );
            assert!(is_declared_env_only(key));
        }
        let _ = assert_storable_key(key);
        let _ = valid_settings_key_shape(key);
        check_secret_redaction(key);
    }
    for value in vars.values().take(3) {
        check_secret_redaction(value);
        let _ = check_setting_value_size(&Value::String(value.clone()));
        let _ = to_env_string(&Value::String(value.clone()));
    }
    // Wrong JSON types through the same ceiling and renderer the website
    // actions use: numbers, bools, null, arrays and objects must not panic,
    // hang or allocate beyond the input.
    let _ = check_setting_value_size(&Value::Null);
    let _ = check_setting_value_size(&Value::Bool(true));
    let _ = check_setting_value_size(&Value::from(0_u64));
    let _ = to_env_string(&Value::Null);
    let _ = to_env_string(&Value::Bool(true));

    if let Some(key) = vars.keys().next() {
        let mut body = Map::new();
        body.insert("key".to_owned(), Value::String(key.clone()));
        let _ = require_settings_key(&body, "settings.get");
        let _ = require_settings_key(&body, "settings.set");
    }
    let mut wrong_type = Map::new();
    wrong_type.insert("key".to_owned(), Value::from(1_u64));
    let _ = require_settings_key(&wrong_type, "settings.get");
    let mut missing = Map::new();
    missing.insert("other".to_owned(), Value::String("x".to_owned()));
    let _ = require_settings_key(&missing, "settings.set");
}

fn drive_all(vars: &HashMap<String, String>) {
    drive_assistant(vars);
    drive_guild_gates(vars);
    drive_internal(vars);
    drive_settings_catalogue(vars);
}

fuzz_target!(|data: &[u8]| {
    let Some(vars) = decode_map(data) else {
        return;
    };
    drive_all(&vars);

    // Unknown keys must be ignored: filtering to the known census must not
    // change any outcome. A mismatch means a parser started reading a new
    // key without classifying it (fail-closed tripwire).
    let filtered: HashMap<String, String> = vars
        .iter()
        .filter(|(key, _)| KNOWN_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let full_assistant = AssistantConfig::from_map(&vars);
    let filtered_assistant = AssistantConfig::from_map(&filtered);
    assert_eq!(full_assistant, filtered_assistant);
    assert_eq!(
        FeatureGates::from_map(&vars).is_ok(),
        FeatureGates::from_map(&filtered).is_ok()
    );
    assert_eq!(
        ModerationGates::from_map(&vars).is_ok(),
        ModerationGates::from_map(&filtered).is_ok()
    );
    assert_eq!(VoiceGates::from_map(&vars), VoiceGates::from_map(&filtered));
    assert_eq!(
        AutomodConfig::from_map(&vars).is_ok(),
        AutomodConfig::from_map(&filtered).is_ok()
    );
});
