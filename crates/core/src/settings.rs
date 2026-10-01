//! Guild-settings hot reload: catalogue, cache, and poll decisions.
//!
//! Ports `src/core/settingsCatalog.ts` (the ~90-key…117-key `env_only`/`cold`/
//! `hot` classification, TOG-3100) and the DB-free half of `src/core/settings.ts`
//! (`SettingsStore` cache + version poll + env rendering, TOG-3093 slice 1)
//! from legacy two-bot as framework-free data plus pure functions. Storage
//! stays behind the caller: the sqlx store lives in `two-bot-cutover`
//! (`crates/cutover/src/settings.rs`, migrations `0330`–`0331`), which calls
//! into this module; the bot binary's 15 s supervisor poll publishes through
//! [`live_channel`] (TOG-10898) — this module builds no dispatcher and no
//! HTTP client.
//!
//! Classes (parity §7): `hot` keys reload from `guild_settings` without a
//! restart, `cold` keys are storable but need a restart before consumers pick
//! them up, `env_only` keys never come from the DB. Unknown keys are refused,
//! not allowed (fail-closed): [`is_env_only_key`] returns true for any name the
//! catalogue has never heard of, so the next capability gate somebody adds is
//! refused until it is classified here.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - catalogue: `src/core/settingsCatalog.ts` (`SETTING_CLASSES`,
//!   `ENV_ONLY_KEY_PREFIXES`, `SECRET_NAMES_NOT_IN_SRC_GREP`, `HOT_WIRED`,
//!   `classifyKey`, `isEnvOnlyKey`, `isDeclaredEnvOnly`)
//! - store: `src/core/settings.ts` (`SettingsStore`, `isStorableKey`,
//!   `assertStorableKey`, `EnvOnlyKeyError`, `toEnvString`)
//! - wiring: `src/index.ts` (15 s `max(version)` poll, `storeFirst` rebuild,
//!   `setting_changed` per hot-wired key) + `src/core/config.ts`
//!   (`storeFirst`, `HOT_WIRED_FIELDS`)
//! - schema: `migrations/0026_guild_settings.sql`,
//!   `0027_guild_settings_env_only.sql`, `0029`/`0031`/`0032` (rota),
//!   `0034` (staging restart), `0039` (redirect trusted proxies)
//!
//! Deliberately out of scope: the full `loadConfig` store-first rebuild (the
//! two-bot-next [`crate::Config`] is still the S1 skeleton; feature flags
//! travel with their feature card), the `settings.get`/`settings.set`
//! website-verb handlers (TOG-9880), and temp-voice runtime (owned by the
//! parent — the `TEMP_VOICE_*` catalogue rows below are classification only).

use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::watch;

/// How often the version poll runs (legacy `pollSeconds ?? 15`, TOG-3093
/// ADR §2.1). The poll reads a transactional revision and row count; sequence
/// maxima cannot detect writes that commit out of allocation order.
pub const POLL_SECONDS: u64 = 15;

/// Prefix test, kept alongside the exact-name table (legacy
/// `ENV_ONLY_KEY_PREFIXES`): the next gate added inside the `TWO_INTERNAL_*`
/// namespace is refused without anyone having to remember this file.
pub const ENV_ONLY_KEY_PREFIXES: &[&str] = &["TWO_INTERNAL_"];

/// Where a setting is allowed to live and when a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingClass {
    /// Dashboard-writable and eligible to apply without a restart.
    Hot,
    /// Storable and safe, but read once at boot: applies at next restart.
    Cold,
    /// Secrets, boot inputs, network binds, and capability gates. Never
    /// stored, never rendered; refused on write and ignored on read.
    EnvOnly,
}

/// The whole census: exactly the names legacy `src/` reads, classified.
/// `cold` and `env_only` carry the legacy citation; `hot` is the residual
/// class (a key is hot when nothing reads it at boot).
pub const SETTING_CLASSES: &[(&str, SettingClass)] = &[
    // --- secrets ---
    ("DISCORD_BOT_TOKEN", SettingClass::EnvOnly),
    ("DISCORD_TOKEN", SettingClass::EnvOnly),
    ("TWO_MODERATION_AUDIT_SECRET", SettingClass::EnvOnly),
    ("TWO_DATABASE_URL", SettingClass::EnvOnly),
    ("TWO_STAGING_DATABASE_URL", SettingClass::EnvOnly),
    ("DISCORD_STAGING_BOT_TOKEN", SettingClass::EnvOnly),
    ("TWO_BACKUP_S3_ACCESS_KEY_ID", SettingClass::EnvOnly),
    ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", SettingClass::EnvOnly),
    // Backup destination: a web-settable bucket turns backups into exfil.
    ("TWO_BACKUP_S3_BUCKET", SettingClass::EnvOnly),
    ("TWO_BACKUP_S3_ENDPOINT", SettingClass::EnvOnly),
    ("TWO_BACKUP_S3_PREFIX", SettingClass::EnvOnly),
    ("TWO_BACKUP_S3_REGION", SettingClass::EnvOnly),
    // --- boot: read before the store exists, or used to find it ---
    ("CREDENTIALS_DIRECTORY", SettingClass::EnvOnly),
    ("TWO_DB_POOL_MAX", SettingClass::EnvOnly),
    ("DISCORD_GUILD_ID", SettingClass::EnvOnly),
    ("DISCORD_STAGING_GUILD_ID", SettingClass::EnvOnly),
    ("TWO_STAGING_RESTART_CONTAINMENT", SettingClass::EnvOnly),
    (
        "TWO_STAGING_RESTART_SYNTHETIC_ACTORS",
        SettingClass::EnvOnly,
    ),
    // --- network binds + the Discord API host ---
    ("TWO_HEALTH_BIND_HOST", SettingClass::EnvOnly),
    ("TWO_HEALTH_PORT", SettingClass::EnvOnly),
    ("TWO_REDIRECT_BIND_HOST", SettingClass::EnvOnly),
    ("TWO_REDIRECT_PORT", SettingClass::EnvOnly),
    ("TWO_REDIRECT_TRUSTED_PROXIES", SettingClass::EnvOnly),
    ("DISCORD_API_BASE", SettingClass::EnvOnly),
    // --- capability gates: the TWO_INTERNAL_* namespace (prefix-covered too,
    // --- so the census stays complete and the drift test can see them) ---
    ("TWO_INTERNAL_ACTIONS", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ALLOW_ADD_MEMBER", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ALLOW_AUTOMATIONS", SettingClass::EnvOnly),
    (
        "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE",
        SettingClass::EnvOnly,
    ),
    ("TWO_INTERNAL_ALLOW_EVENT_CANCEL", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ALLOW_EVENT_READ", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ALLOW_MODERATION", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ALLOW_SETTINGS", SettingClass::EnvOnly),
    ("TWO_INTERNAL_BIND_HOST", SettingClass::EnvOnly),
    ("TWO_INTERNAL_CHANNEL_KEYS", SettingClass::EnvOnly),
    ("TWO_INTERNAL_PORT", SettingClass::EnvOnly),
    ("TWO_INTERNAL_ROLE_KEYS", SettingClass::EnvOnly),
    // --- capability gates outside the namespace (TOG-3183 finding) ---
    ("TWO_MODERATION", SettingClass::EnvOnly),
    ("TWO_ONBOARDING_MODE", SettingClass::EnvOnly),
    // --- staging-only rota collection/notice (DROPped as runtime, but the
    // --- refusal stays: never dashboard-settable) ---
    ("TWO_ONBOARDING_ROTA_MEASUREMENT", SettingClass::EnvOnly),
    ("TWO_ONBOARDING_ROTA_NOTICE", SettingClass::EnvOnly),
    ("TWO_ONBOARDING_ROTA_PSEUDONYM_KEY", SettingClass::EnvOnly),
    (
        "TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID",
        SettingClass::EnvOnly,
    ),
    ("TWO_ONBOARDING_ROTA_READER_IDS", SettingClass::EnvOnly),
    // --- verb reach bounds: who moderation may not touch, who anti-nuke
    // --- ignores ---
    ("TWO_MODERATION_PROTECTED_ROLE_IDS", SettingClass::EnvOnly),
    ("TWO_OWEN_USER_ID", SettingClass::EnvOnly),
    ("TWO_ANTI_NUKE_PROTECTED_USER_IDS", SettingClass::EnvOnly),
    ("TWO_ANTI_NUKE_TRUSTED_USER_IDS", SettingClass::EnvOnly),
    // A filesystem path chosen by a web form is a write primitive.
    ("TWO_ANTI_NUKE_SNAPSHOT_PATH", SettingClass::EnvOnly),
    // --- cold: read once at boot ---
    ("TWO_AUTOMOD", SettingClass::Cold),
    ("TWO_ANNOUNCEMENTS", SettingClass::Cold),
    ("TWO_AUTOMATIONS", SettingClass::Cold),
    ("TWO_TEXT_COMMANDS", SettingClass::Cold),
    ("TEMP_VOICE_ENABLED", SettingClass::Cold),
    ("TWO_TEMP_VOICE", SettingClass::Cold),
    ("TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID", SettingClass::Cold),
    ("TWO_TEMP_VOICE_CATEGORY_ID", SettingClass::Cold),
    ("TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS", SettingClass::Cold),
    ("TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS", SettingClass::Cold),
    ("TWO_TEMP_VOICE_SWEEP_SECONDS", SettingClass::Cold),
    ("TWO_TEMP_VOICE_MAX_PER_USER", SettingClass::Cold),
    ("TWO_TEMP_VOICE_MAX_PER_GUILD", SettingClass::Cold),
    ("TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS", SettingClass::Cold),
    ("TWO_TEMP_VOICE_NAME_TEMPLATE", SettingClass::Cold),
    ("TWO_TEMP_VOICE_PANEL_CHANNEL_ID", SettingClass::Cold),
    ("TWO_TEMP_VOICE_DISABLED_CONTROLS", SettingClass::Cold),
    ("TWO_ANTI_NUKE", SettingClass::Cold),
    ("TWO_ANTI_NUKE_DRY_RUN", SettingClass::Cold),
    ("TWO_COMMUNITY_SCORECARD", SettingClass::Cold),
    ("TWO_COMMUNITY_RECOMMENDATIONS", SettingClass::Cold),
    ("TWO_COMMUNITY_CORRECTION_CYCLES", SettingClass::Cold),
    ("TWO_SELF_ROLE_PANELS", SettingClass::Cold),
    ("TWO_PRESENCE_PROBE", SettingClass::Cold),
    ("TWO_FEED_POLL_SECONDS", SettingClass::Cold),
    ("TWO_INACTIVITY_DAYS", SettingClass::Cold),
    ("TWO_TICKET_COOLDOWN_SECONDS", SettingClass::Cold),
    ("LOG_LEVEL", SettingClass::Cold),
    // --- hot: channel/role ids, thresholds, lists, dry-runs ---
    ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_AUDIT_LOG_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_GOODBYE_CHANNEL_IDS", SettingClass::Hot),
    ("DISCORD_LANDING_CHANNEL_IDS", SettingClass::Hot),
    ("DISCORD_MODERATION_LOG_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID", SettingClass::Hot),
    (
        "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID",
        SettingClass::Hot,
    ),
    ("DISCORD_STAFF_ALERT_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_TICKET_CATEGORY_ID", SettingClass::Hot),
    ("DISCORD_TICKET_PANEL_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_VOICE_LOG_CHANNEL_ID", SettingClass::Hot),
    ("DISCORD_TICKET_STAFF_ROLE_ID", SettingClass::Hot),
    ("TWO_RAID_JOIN_THRESHOLD", SettingClass::Hot),
    ("TWO_RAID_WINDOW_SECONDS", SettingClass::Hot),
    ("TWO_JOIN_RISK_THRESHOLD", SettingClass::Hot),
    ("TWO_JOIN_RISK_WINDOW_SECONDS", SettingClass::Hot),
    ("TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS", SettingClass::Hot),
    ("TWO_ANTI_NUKE_HEAT_THRESHOLD", SettingClass::Hot),
    ("TWO_ANTI_NUKE_WINDOW_SECONDS", SettingClass::Hot),
    ("TWO_BULK_JOIN_WINDOW_UNTIL", SettingClass::Hot),
    ("TWO_AUTOMOD_ALLOWED_DOMAINS", SettingClass::Hot),
    ("TWO_AUTOMOD_BAD_WORDS", SettingClass::Hot),
    (
        "TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS",
        SettingClass::Hot,
    ),
    ("TWO_AUTOMOD_BYPASS_ROLE_IDS", SettingClass::Hot),
    ("TWO_AUTOMOD_ENFORCE", SettingClass::Hot),
    ("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS", SettingClass::Hot),
    ("TWO_AUTOMOD_MENTION_LIMIT", SettingClass::Hot),
    ("TWO_AUTOMOD_REPEAT_COUNT", SettingClass::Hot),
    ("TWO_AUTOMOD_REPEAT_WINDOW_SECONDS", SettingClass::Hot),
    ("TWO_AUTOMOD_SANCTIONS", SettingClass::Hot),
    ("TWO_COMMUNITY_AUTOMATION_ACTOR_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_CLASSIFIER_VERSION", SettingClass::Hot),
    ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_RAID_ACTOR_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_STAGING_ACTOR_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_STAGING_GUILD_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_TEST_ACTOR_IDS", SettingClass::Hot),
    ("TWO_COMMUNITY_WELCOME_CHANNEL_IDS", SettingClass::Hot),
    ("TWO_ONBOARDING_DRY_RUN", SettingClass::Hot),
    ("TWO_SELF_ROLE_DRY_RUN", SettingClass::Hot),
    ("TWO_REDIRECT_FALLBACK_CODE", SettingClass::Hot),
];

/// The hot keys whose consumers read through the live config (legacy
/// `HOT_WIRED`). Hot is a permission, not a promise: only keys listed here
/// get a `setting_changed` log line and live application on reload; other hot
/// keys are stored and snapshotted but read at consumer construction until
/// their consumer is converted.
pub const HOT_WIRED: &[&str] = &[
    "TWO_RAID_JOIN_THRESHOLD",
    "TWO_RAID_WINDOW_SECONDS",
    "DISCORD_LANDING_CHANNEL_IDS",
    "DISCORD_GOODBYE_CHANNEL_IDS",
    "TWO_AUTOMOD_REPEAT_COUNT",
];

/// The catalogue class for `key`, or `None` for a name this file has never
/// heard of (legacy `classifyKey`).
#[must_use]
pub fn classify_key(key: &str) -> Option<SettingClass> {
    SETTING_CLASSES
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, class)| *class)
}

/// True for any key that must stay in the environment (legacy `isEnvOnlyKey`).
/// Unknown keys are env-only: fail-closed is the only default that survives
/// the codebase growing.
#[must_use]
pub fn is_env_only_key(key: &str) -> bool {
    if ENV_ONLY_KEY_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return true;
    }
    !matches!(
        classify_key(key),
        Some(SettingClass::Hot | SettingClass::Cold)
    )
}

/// True when the catalogue *declares* the key environment-only, as opposed to
/// refusing it for never having heard of it (legacy `isDeclaredEnvOnly`).
/// Nothing that enforces branches on this; it exists for the things that
/// report to a human ("environment-only" vs "no such setting").
#[must_use]
pub fn is_declared_env_only(key: &str) -> bool {
    if ENV_ONLY_KEY_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return true;
    }
    matches!(classify_key(key), Some(SettingClass::EnvOnly))
}

/// False for any key that must stay in the environment (legacy
/// `isStorableKey`): env-only, prefix-refused, and unknown keys alike.
#[must_use]
pub fn is_storable_key(key: &str) -> bool {
    !is_env_only_key(key)
}

/// A refused settings write: the key must stay in the environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{key} is environment-only and must never be stored in guild_settings. \
     Secrets, boot inputs, network binds and the keys that gate what the \
     website may make the bot do stay in the environment; a key absent from \
     the settings catalogue is refused for the same reason — classify it first."
)]
pub struct EnvOnlyKeyError {
    /// The refused key name (never a value).
    pub key: String,
}

/// Throws [`EnvOnlyKeyError`] rather than returning false. For write paths:
/// the check runs before any SQL so a refusal writes no row and no audit row.
pub fn assert_storable_key(key: &str) -> Result<(), EnvOnlyKeyError> {
    if is_storable_key(key) {
        Ok(())
    } else {
        Err(EnvOnlyKeyError {
            key: key.to_owned(),
        })
    }
}

/// A stored JSON value rendered the way the environment would have carried it,
/// because every reader was written against an environment string (legacy
/// `toEnvString`). Booleans become `'1'`/`'0'` rather than `'true'`/`'false'`:
/// every boolean env var is tested with `== '1'`, so `'true'` would store
/// cleanly, read back cleanly, and silently mean off. Values that render to
/// `None` (JSON null) are omitted by snapshots so they fall through to the
/// environment instead of masking it with `""`.
#[must_use]
pub fn to_env_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(if *b { "1".to_owned() } else { "0".to_owned() }),
        Value::Number(n) => {
            // JSON/JSONB can carry integer thresholds as 3.0 or 3e0, but env
            // readers require "3". Never send native i64/u64 values through
            // f64: snowflakes above 2^53 must retain every digit.
            if n.is_f64() {
                let f = n.as_f64()?;
                if f.fract() == 0.0 {
                    return Some(if f == 0.0 {
                        "0".to_owned()
                    } else {
                        format!("{f:.0}")
                    });
                }
            }
            Some(n.to_string())
        }
        Value::String(s) => Some(s.clone()),
        // Id lists are comma-separated in the environment and stay that way,
        // so the dashboard can store a real array without every reader
        // learning a second shape. Unrenderable elements become "".
        Value::Array(items) => Some(
            items
                .iter()
                .map(|v| to_env_string(v).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Object(_) => serde_json::to_string(value).ok(),
    }
}

/// One `guild_settings` row as plain data (legacy `SettingRow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    pub guild_id: String,
    pub key: String,
    pub value: Value,
    pub version: i64,
}

/// Rows and commit-safe revision from the same database snapshot. The revision
/// is not `max(row.version)`: sequence allocation order is not commit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub revision: i64,
    pub rows: Vec<SettingRow>,
}

/// In-memory cache over `guild_settings`, refreshed by the version poll
/// (legacy `SettingsStore` minus the database: reads are synchronous and never
/// touch the DB because the callers are gateway handlers on a hot path;
/// staleness is bounded by [`POLL_SECONDS`]).
#[derive(Debug, Clone, Default)]
pub struct SettingsCache {
    entries: HashMap<(String, String), Value>,
    revision: i64,
    row_count: usize,
}

impl SettingsCache {
    /// Fill (or refill) the cache from a consistent snapshot. Call once before
    /// reading, and again on every poll that moved.
    #[must_use]
    pub fn load(snapshot: &SettingsSnapshot) -> Self {
        let mut cache = Self::default();
        cache.rebuild(snapshot);
        cache
    }

    fn rebuild(&mut self, snapshot: &SettingsSnapshot) {
        self.entries = snapshot
            .rows
            .iter()
            .map(|row| ((row.guild_id.clone(), row.key.clone()), row.value.clone()))
            .collect();
        self.revision = snapshot.revision;
        self.row_count = snapshot.rows.len();
    }

    /// Transactional revision of the snapshot (`0` before the first load).
    #[must_use]
    pub fn revision(&self) -> i64 {
        self.revision
    }

    /// Rows the cache was built from.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Rows currently cached, across all guilds.
    #[must_use]
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// The stored JSON value, or `None` when the key falls through to env.
    #[must_use]
    pub fn get(&self, guild_id: &str, key: &str) -> Option<&Value> {
        if !is_storable_key(key) {
            return None;
        }
        self.entries.get(&(guild_id.to_owned(), key.to_owned()))
    }

    /// The cached settings for one guild, rendered as environment strings.
    /// Env-only and unknown keys are excluded belt-and-braces (the schema
    /// refuses them too); unrenderable values are omitted so they fall through
    /// to the environment. A `None` guild reads nothing rather than picking an
    /// arbitrary guild's settings.
    #[must_use]
    pub fn env_snapshot(&self, guild_id: Option<&str>) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some(guild_id) = guild_id else {
            return out;
        };
        for ((guild, key), value) in &self.entries {
            if guild != guild_id || !is_storable_key(key) {
                continue;
            }
            if let Some(rendered) = to_env_string(value) {
                out.insert(key.clone(), rendered);
            }
        }
        out
    }

    /// Refetch when the transactional revision or row count moves. `!=` rather
    /// than `>` also reloads a restored backup with a lower revision.
    #[must_use]
    pub fn needs_refresh(&self, latest_revision: i64, latest_count: i64) -> bool {
        latest_revision != self.revision || latest_count != self.row_count as i64
    }

    /// Rebuild the cache from a consistent snapshot and report what moved,
    /// partitioned by class: hot changes apply live, cold changes are stored
    /// but need a restart, env-only/unknown rows are ignored with a log line
    /// (legacy `refreshIfChanged` + the `HOT_WIRED_FIELDS` change log).
    pub fn refresh(&mut self, snapshot: &SettingsSnapshot) -> RefreshReport {
        let from_revision = self.revision;
        let from_rows = self.row_count;
        let rows = &snapshot.rows;

        // `new` borrows `rows`; deletions read `self.entries` directly, so no
        // intermediate borrowed-key map is needed.
        let mut new: HashMap<(String, String), &Value> = HashMap::with_capacity(rows.len());
        for row in rows {
            new.insert((row.guild_id.clone(), row.key.clone()), &row.value);
        }

        let mut report = RefreshReport {
            from_revision,
            from_rows,
            to_revision: snapshot.revision,
            to_rows: rows.len(),
            changed: false,
            hot: Vec::new(),
            cold: Vec::new(),
            ignored: Vec::new(),
        };

        for (id, old_value) in &self.entries {
            if !new.contains_key(id) {
                report.push_change(id, Some(old_value), None);
            }
        }
        for (id, new_value) in &new {
            match self.entries.get(id) {
                Some(old_value) if *old_value == **new_value => {}
                Some(old_value) => report.push_change(id, Some(old_value), Some(new_value)),
                None => report.push_change(id, None, Some(new_value)),
            }
        }

        self.rebuild(snapshot);
        report.changed = self.revision != from_revision || self.row_count != from_rows;
        report
    }
}

/// One consumer-visible change: old/new as environment strings (`None` = the
/// key had no row, i.e. it was reading from the environment / handed back to
/// it by a delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChange {
    pub guild_id: String,
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Why a stored row is never applied to the live config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Declared `env_only` (or prefix-refused): a direct-SQL bypass of the
    /// write guard and both CHECK constraints.
    EnvOnly,
    /// No catalogue entry at all: fail-closed.
    Unknown,
}

/// A stored row the poll refuses to apply (legacy belt-and-braces: the write
/// guard, two CHECK constraints, and this read-side refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredChange {
    pub guild_id: String,
    pub key: String,
    pub reason: IgnoreReason,
}

/// What one poll that moved found, partitioned for logging and application
/// (legacy `settings_reloaded` + `setting_changed` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    pub from_revision: i64,
    pub to_revision: i64,
    pub from_rows: usize,
    pub to_rows: usize,
    /// The poll moved (version or row count changed).
    pub changed: bool,
    /// Hot-wired changes to apply live now (one `setting_changed` line each).
    pub hot: Vec<KeyChange>,
    /// Cold changes: stored, visible in snapshots, need a restart.
    pub cold: Vec<KeyChange>,
    /// Env-only/unknown rows: never applied, one log line each.
    pub ignored: Vec<IgnoredChange>,
}

impl RefreshReport {
    fn push_change(&mut self, id: &(String, String), old: Option<&Value>, new: Option<&Value>) {
        let (guild_id, key) = (id.0.clone(), id.1.clone());
        match classify_key(&key) {
            Some(SettingClass::Hot) if HOT_WIRED.contains(&key.as_str()) => {
                self.hot.push(KeyChange {
                    guild_id,
                    key,
                    old: old.and_then(to_env_string),
                    new: new.and_then(to_env_string),
                });
            }
            Some(SettingClass::Hot) | Some(SettingClass::Cold) => {
                // Hot-but-unwired keys are stored and snapshotted like cold:
                // the dashboard must render them as "next restart" until their
                // consumer is converted to read live (legacy HOT_WIRED note).
                self.cold.push(KeyChange {
                    guild_id,
                    key,
                    old: old.and_then(to_env_string),
                    new: new.and_then(to_env_string),
                });
            }
            Some(SettingClass::EnvOnly) | None => {
                let reason = if is_declared_env_only(&key) {
                    IgnoreReason::EnvOnly
                } else {
                    IgnoreReason::Unknown
                };
                // Prefix-refused keys are declared env-only, never unknown.
                self.ignored.push(IgnoredChange {
                    guild_id,
                    key,
                    reason,
                });
            }
        }
    }
}

/// The read side of the shared hot-settings handle (TOG-10898). One
/// `tokio::sync::watch` channel carries an immutable `Arc<SettingsCache>`;
/// readers borrow the current snapshot without lock contention on the writer
/// and never observe a half-rebuilt cache — the swap is a single atomic
/// store (arc-swap semantics without a new dependency).
///
/// Before the first publish, readers see the empty revision-0 cache: every
/// lookup falls through to the environment exactly as if the table were
/// empty, which is also the behaviour while the poll job is parked.
#[derive(Debug, Clone)]
pub struct LiveSettings {
    rx: watch::Receiver<Arc<SettingsCache>>,
}

/// The write side, owned by the single supervisor poll task. Mutable refresh
/// state stays on the writer; readers only ever see a fully published
/// snapshot.
#[derive(Debug)]
pub struct LiveSettingsWriter {
    tx: watch::Sender<Arc<SettingsCache>>,
    cache: SettingsCache,
}

/// Build the writer/reader pair: the poll job keeps the writer, feature
/// runtimes clone the reader.
#[must_use]
pub fn live_channel() -> (LiveSettingsWriter, LiveSettings) {
    let (tx, rx) = watch::channel(Arc::new(SettingsCache::default()));
    (
        LiveSettingsWriter {
            tx,
            cache: SettingsCache::default(),
        },
        LiveSettings { rx },
    )
}

impl LiveSettingsWriter {
    /// True when the polled marks moved (revision or row count); the caller
    /// then loads a consistent snapshot and hands it to [`Self::publish`].
    #[must_use]
    pub fn needs_refresh(&self, revision: i64, rows: i64) -> bool {
        self.cache.needs_refresh(revision, rows)
    }

    /// Rebuild the working cache from one consistent snapshot and publish the
    /// new `Arc` to every reader when anything moved. Returns the partitioned
    /// report for logging. The caller must refuse malformed snapshots before
    /// this runs: publish is all-or-nothing.
    pub fn publish(&mut self, snapshot: &SettingsSnapshot) -> RefreshReport {
        let report = self.cache.refresh(snapshot);
        if report.changed {
            self.tx.send_replace(Arc::new(self.cache.clone()));
        }
        report
    }

    /// Revision of the writer's working cache (`0` before the first publish).
    #[must_use]
    pub fn revision(&self) -> i64 {
        self.cache.revision()
    }
}

impl LiveSettings {
    /// The latest published snapshot (empty, revision 0, before the first
    /// successful poll — reads fall through to the environment).
    #[must_use]
    pub fn snapshot(&self) -> Arc<SettingsCache> {
        Arc::clone(&self.rx.borrow())
    }

    /// Transactional revision of the published snapshot (`0` before the first
    /// publish).
    #[must_use]
    pub fn revision(&self) -> i64 {
        self.rx.borrow().revision()
    }

    /// One stored value for a guild, or `None` to fall through to the
    /// environment. Hot and cold classes are both visible; env-only and
    /// unknown keys are refused by [`SettingsCache::get`] itself.
    #[must_use]
    pub fn get(&self, guild_id: &str, key: &str) -> Option<Value> {
        self.rx.borrow().get(guild_id, key).cloned()
    }

    /// One guild's env-shaped view — the input a store-first config consumer
    /// (legacy `storeFirst`) layers under the process environment.
    #[must_use]
    pub fn env_snapshot(&self, guild_id: Option<&str>) -> HashMap<String, String> {
        self.rx.borrow().env_snapshot(guild_id)
    }
}

/// A validated settings write, ready for the store to execute (legacy
/// `SettingsStore.set` up to — but not including — the SQL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWrite {
    pub guild_id: String,
    pub key: String,
    /// Hot or cold. Cold writes are stored and audited exactly like hot ones;
    /// the caller logs that they need a restart.
    pub class: SettingClass,
    pub action: WriteAction,
    pub actor: String,
}

/// Upsert a value, or delete the row (`value === null`, which hands the key
/// back to the environment — the documented undo path, audited like any other
/// change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAction {
    Upsert(Value),
    Delete,
}

/// Why a write was refused before issuing any SQL (so a refusal writes no row
/// and — the part that matters — no audit row either).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefusal {
    #[error("refused: {0} is environment-only")]
    EnvOnly(EnvOnlyKeyError),
    #[error("refused: no such setting (classify it in the settings catalogue first): {0}")]
    Unknown(String),
    #[error("refused: settings writes must name an actor")]
    MissingActor,
}

/// Validate a write: key guard first (env-only and unknown never reach SQL),
/// then attribution (an unattributed change is the one kind of row the audit
/// table exists to make impossible).
pub fn validate_write(
    guild_id: &str,
    key: &str,
    value: Option<Value>,
    actor: &str,
) -> Result<ValidatedWrite, WriteRefusal> {
    // Prefix first, catalogue second (legacy `isEnvOnlyKey` order): a gate
    // added inside the TWO_INTERNAL_* namespace is env-only even before
    // anyone classifies it. Only a non-prefixed, uncatalogued name is
    // "unknown" rather than declared env-only.
    if ENV_ONLY_KEY_PREFIXES.iter().any(|p| key.starts_with(p)) {
        return Err(WriteRefusal::EnvOnly(EnvOnlyKeyError {
            key: key.to_owned(),
        }));
    }
    let class = classify_key(key).ok_or_else(|| WriteRefusal::Unknown(key.to_owned()))?;
    if class == SettingClass::EnvOnly {
        return Err(WriteRefusal::EnvOnly(EnvOnlyKeyError {
            key: key.to_owned(),
        }));
    }
    if actor.is_empty() {
        return Err(WriteRefusal::MissingActor);
    }
    Ok(ValidatedWrite {
        guild_id: guild_id.to_owned(),
        key: key.to_owned(),
        class,
        action: match value {
            Some(value) => WriteAction::Upsert(value),
            None => WriteAction::Delete,
        },
        actor: actor.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The catalogue tripwire (legacy `test/unit.settingscatalog.test.ts` in
    // spirit): every key pinned to its class in both directions. A key added
    // to the implementation but missing here fails the count; a key removed
    // from the implementation but present here fails the lookup; a reclassified
    // key fails the equality. Duplication is the point.
    const EXPECTED_HOT: &[&str] = &[
        "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
        "DISCORD_AUDIT_LOG_CHANNEL_ID",
        "DISCORD_GOODBYE_CHANNEL_IDS",
        "DISCORD_LANDING_CHANNEL_IDS",
        "DISCORD_MODERATION_LOG_CHANNEL_ID",
        "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID",
        "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID",
        "DISCORD_STAFF_ALERT_CHANNEL_ID",
        "DISCORD_TICKET_CATEGORY_ID",
        "DISCORD_TICKET_PANEL_CHANNEL_ID",
        "DISCORD_TICKET_STAFF_ROLE_ID",
        "DISCORD_VOICE_LOG_CHANNEL_ID",
        "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS",
        "TWO_ANTI_NUKE_HEAT_THRESHOLD",
        "TWO_ANTI_NUKE_WINDOW_SECONDS",
        "TWO_AUTOMOD_ALLOWED_DOMAINS",
        "TWO_AUTOMOD_BAD_WORDS",
        "TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS",
        "TWO_AUTOMOD_BYPASS_ROLE_IDS",
        "TWO_AUTOMOD_ENFORCE",
        "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS",
        "TWO_AUTOMOD_MENTION_LIMIT",
        "TWO_AUTOMOD_REPEAT_COUNT",
        "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS",
        "TWO_AUTOMOD_SANCTIONS",
        "TWO_BULK_JOIN_WINDOW_UNTIL",
        "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS",
        "TWO_COMMUNITY_CLASSIFIER_VERSION",
        "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
        "TWO_COMMUNITY_RAID_ACTOR_IDS",
        "TWO_COMMUNITY_STAGING_ACTOR_IDS",
        "TWO_COMMUNITY_STAGING_GUILD_IDS",
        "TWO_COMMUNITY_TEST_ACTOR_IDS",
        "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
        "TWO_JOIN_RISK_THRESHOLD",
        "TWO_JOIN_RISK_WINDOW_SECONDS",
        "TWO_ONBOARDING_DRY_RUN",
        "TWO_RAID_JOIN_THRESHOLD",
        "TWO_RAID_WINDOW_SECONDS",
        "TWO_REDIRECT_FALLBACK_CODE",
        "TWO_SELF_ROLE_DRY_RUN",
    ];

    const EXPECTED_COLD: &[&str] = &[
        "LOG_LEVEL",
        "TEMP_VOICE_ENABLED",
        "TWO_ANNOUNCEMENTS",
        "TWO_ANTI_NUKE",
        "TWO_ANTI_NUKE_DRY_RUN",
        "TWO_AUTOMATIONS",
        "TWO_AUTOMOD",
        "TWO_COMMUNITY_CORRECTION_CYCLES",
        "TWO_COMMUNITY_RECOMMENDATIONS",
        "TWO_COMMUNITY_SCORECARD",
        "TWO_FEED_POLL_SECONDS",
        "TWO_INACTIVITY_DAYS",
        "TWO_PRESENCE_PROBE",
        "TWO_SELF_ROLE_PANELS",
        "TWO_TEMP_VOICE",
        "TWO_TEMP_VOICE_CATEGORY_ID",
        "TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS",
        "TWO_TEMP_VOICE_DISABLED_CONTROLS",
        "TWO_TEMP_VOICE_EMPTY_GRACE_SECONDS",
        "TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID",
        "TWO_TEMP_VOICE_MAX_PER_GUILD",
        "TWO_TEMP_VOICE_MAX_PER_USER",
        "TWO_TEMP_VOICE_NAME_TEMPLATE",
        "TWO_TEMP_VOICE_PANEL_CHANNEL_ID",
        "TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS",
        "TWO_TEMP_VOICE_SWEEP_SECONDS",
        "TWO_TEXT_COMMANDS",
        "TWO_TICKET_COOLDOWN_SECONDS",
    ];

    const EXPECTED_ENV_ONLY: &[&str] = &[
        "CREDENTIALS_DIRECTORY",
        "DISCORD_API_BASE",
        "DISCORD_BOT_TOKEN",
        "DISCORD_GUILD_ID",
        "DISCORD_STAGING_BOT_TOKEN",
        "DISCORD_STAGING_GUILD_ID",
        "DISCORD_TOKEN",
        "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
        "TWO_ANTI_NUKE_SNAPSHOT_PATH",
        "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
        "TWO_BACKUP_S3_ACCESS_KEY_ID",
        "TWO_BACKUP_S3_BUCKET",
        "TWO_BACKUP_S3_ENDPOINT",
        "TWO_BACKUP_S3_PREFIX",
        "TWO_BACKUP_S3_REGION",
        "TWO_BACKUP_S3_SECRET_ACCESS_KEY",
        "TWO_DATABASE_URL",
        "TWO_DB_POOL_MAX",
        "TWO_HEALTH_BIND_HOST",
        "TWO_HEALTH_PORT",
        "TWO_INTERNAL_ACTIONS",
        "TWO_INTERNAL_ALLOW_ADD_MEMBER",
        "TWO_INTERNAL_ALLOW_AUTOMATIONS",
        "TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE",
        "TWO_INTERNAL_ALLOW_EVENT_CANCEL",
        "TWO_INTERNAL_ALLOW_EVENT_READ",
        "TWO_INTERNAL_ALLOW_MODERATION",
        "TWO_INTERNAL_ALLOW_SETTINGS",
        "TWO_INTERNAL_BIND_HOST",
        "TWO_INTERNAL_CHANNEL_KEYS",
        "TWO_INTERNAL_PORT",
        "TWO_INTERNAL_ROLE_KEYS",
        "TWO_MODERATION",
        "TWO_MODERATION_AUDIT_SECRET",
        "TWO_MODERATION_PROTECTED_ROLE_IDS",
        "TWO_ONBOARDING_MODE",
        "TWO_ONBOARDING_ROTA_MEASUREMENT",
        "TWO_ONBOARDING_ROTA_NOTICE",
        "TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID",
        "TWO_ONBOARDING_ROTA_PSEUDONYM_KEY",
        "TWO_ONBOARDING_ROTA_READER_IDS",
        "TWO_OWEN_USER_ID",
        "TWO_REDIRECT_BIND_HOST",
        "TWO_REDIRECT_PORT",
        "TWO_REDIRECT_TRUSTED_PROXIES",
        "TWO_STAGING_DATABASE_URL",
        "TWO_STAGING_RESTART_CONTAINMENT",
        "TWO_STAGING_RESTART_SYNTHETIC_ACTORS",
    ];

    #[test]
    fn catalogue_pins_every_key_class_in_both_directions() {
        for key in EXPECTED_HOT {
            assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        }
        for key in EXPECTED_COLD {
            assert_eq!(classify_key(key), Some(SettingClass::Cold), "{key}");
        }
        for key in EXPECTED_ENV_ONLY {
            assert_eq!(classify_key(key), Some(SettingClass::EnvOnly), "{key}");
        }
        let expected_total = EXPECTED_HOT.len() + EXPECTED_COLD.len() + EXPECTED_ENV_ONLY.len();
        assert_eq!(expected_total, 117, "tripwire lists must stay complete");
        assert_eq!(
            SETTING_CLASSES.len(),
            expected_total,
            "catalogue grew without updating the tripwire"
        );
        for (key, class) in SETTING_CLASSES {
            let expected = if EXPECTED_HOT.contains(key) {
                SettingClass::Hot
            } else if EXPECTED_COLD.contains(key) {
                SettingClass::Cold
            } else if EXPECTED_ENV_ONLY.contains(key) {
                SettingClass::EnvOnly
            } else {
                panic!("catalogue key missing from tripwire: {key}");
            };
            assert_eq!(*class, expected, "{key}");
        }
    }

    #[test]
    fn unknown_keys_are_refused_not_allowed() {
        assert_eq!(classify_key("TWO_SOMETHING_NEW"), None);
        assert!(is_env_only_key("TWO_SOMETHING_NEW"));
        assert!(!is_storable_key("TWO_SOMETHING_NEW"));
        assert!(!is_declared_env_only("TWO_SOMETHING_NEW"));
        assert!(assert_storable_key("TWO_SOMETHING_NEW").is_err());
    }

    #[test]
    fn internal_prefix_covers_gates_nobody_has_written_yet() {
        assert!(is_env_only_key("TWO_INTERNAL_ALLOW_ANYTHING"));
        assert!(is_declared_env_only("TWO_INTERNAL_ALLOW_ANYTHING"));
        assert!(!is_storable_key("TWO_INTERNAL_ALLOW_ANYTHING"));
        // A lookalike without the exact prefix shape is still refused, but as
        // unknown rather than declared.
        assert!(is_env_only_key("TWO_INTERNALISED_GREETING"));
        assert!(!is_declared_env_only("TWO_INTERNALISED_GREETING"));
    }

    #[test]
    fn hot_wired_is_a_hot_subset() {
        assert_eq!(HOT_WIRED.len(), 5);
        for key in HOT_WIRED {
            assert_eq!(classify_key(key), Some(SettingClass::Hot), "{key}");
        }
    }

    #[test]
    fn poll_interval_is_fifteen_seconds() {
        assert_eq!(POLL_SECONDS, 15);
    }

    #[test]
    fn values_render_the_way_the_environment_carried_them() {
        assert_eq!(to_env_string(&json!(true)), Some("1".to_owned()));
        assert_eq!(to_env_string(&json!(false)), Some("0".to_owned()));
        assert_eq!(to_env_string(&json!(5)), Some("5".to_owned()));
        assert_eq!(to_env_string(&json!("delete")), Some("delete".to_owned()));
        assert_eq!(to_env_string(&json!(["1", "2"])), Some("1,2".to_owned()));
        assert_eq!(to_env_string(&Value::Null), None);
    }

    #[test]
    fn integral_numbers_render_without_decimals_or_integer_precision_loss() {
        for (value, expected) in [
            (json!(3.0), "3"),
            (serde_json::from_str("3e0").unwrap(), "3"),
            (json!(-3.0), "-3"),
            (json!(-0.0), "0"),
            (json!(1e20), "100000000000000000000"),
            (json!(9_007_199_254_740_993_u64), "9007199254740993"),
            (json!(u64::MAX), "18446744073709551615"),
            (json!(i64::MIN), "-9223372036854775808"),
            (json!(i64::MAX), "9223372036854775807"),
            (json!(3.5), "3.5"),
            (json!(-3.5), "-3.5"),
        ] {
            assert_eq!(to_env_string(&value).as_deref(), Some(expected), "{value}");
        }
        assert_eq!(
            to_env_string(&json!([3.0, u64::MAX])).as_deref(),
            Some("3,18446744073709551615")
        );
    }

    #[test]
    fn integral_settings_snapshots_feed_the_automod_integer_reader() {
        for literal in ["3", "3.0", "3e0"] {
            let cache = SettingsCache::load(&snapshot(
                1,
                vec![row(
                    "g1",
                    "TWO_AUTOMOD_REPEAT_COUNT",
                    serde_json::from_str(literal).unwrap(),
                    1,
                )],
            ));
            let config = crate::AutomodConfig::from_map(&cache.env_snapshot(Some("g1")))
                .expect("integral JSON settings must parse as integer env values");
            assert_eq!(config.policy.repeated_message_count, 3, "{literal}");
        }
        let cache = SettingsCache::load(&snapshot(
            1,
            vec![row("g1", "TWO_AUTOMOD_REPEAT_COUNT", json!(3.5), 1)],
        ));
        assert!(crate::AutomodConfig::from_map(&cache.env_snapshot(Some("g1"))).is_err());
    }

    fn row(guild: &str, key: &str, value: Value, version: i64) -> SettingRow {
        SettingRow {
            guild_id: guild.to_owned(),
            key: key.to_owned(),
            value,
            version,
        }
    }

    fn snapshot(revision: i64, rows: Vec<SettingRow>) -> SettingsSnapshot {
        SettingsSnapshot { revision, rows }
    }

    #[test]
    fn hot_change_is_visible_within_one_poll() {
        let mut cache = SettingsCache::default();
        assert!(!cache.needs_refresh(0, 0));
        let loaded = snapshot(1, vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3), 1)]);
        assert!(cache.needs_refresh(1, 1));
        let report = cache.refresh(&loaded);
        assert!(report.changed);
        assert_eq!(cache.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(&json!(3)));
        assert_eq!(report.hot.len(), 1);
        assert_eq!(report.hot[0].key, "TWO_RAID_JOIN_THRESHOLD");
        assert_eq!(report.hot[0].new.as_deref(), Some("3"));
        assert!(report.cold.is_empty() && report.ignored.is_empty());
        assert!(!cache.needs_refresh(1, 1), "second poll is a no-op");
    }

    #[test]
    fn revision_detects_changes_with_unchanged_row_maximum_and_count() {
        let mut cache = SettingsCache::load(&snapshot(
            1,
            vec![
                row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(1), 1),
                row("g1", "TWO_RAID_WINDOW_SECONDS", json!(9), 4),
            ],
        ));
        // Sequence value 2 commits after 4. The transactional revision moves
        // even though max(row.version) and count remain (4, 2).
        let loaded = snapshot(
            2,
            vec![
                row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(2), 2),
                row("g1", "TWO_RAID_WINDOW_SECONDS", json!(9), 4),
            ],
        );
        assert!(cache.needs_refresh(2, 2));
        let report = cache.refresh(&loaded);
        assert!(report.changed);
        assert_eq!((report.from_revision, report.to_revision), (1, 2));
        assert_eq!(cache.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(&json!(2)));
        assert!(!cache.needs_refresh(2, 2));
        assert!(cache.needs_refresh(0, 2), "restored revisions reload too");
    }

    #[test]
    fn delete_and_reinsert_are_visible_even_with_unchanged_count() {
        let mut cache = SettingsCache::load(&snapshot(
            1,
            vec![
                row("g1", "TWO_ONBOARDING_DRY_RUN", json!(true), 1),
                row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(9), 4),
            ],
        ));
        let loaded = snapshot(
            3,
            vec![
                row("g1", "TWO_AUTOMOD_REPEAT_COUNT", json!(2), 2),
                row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(9), 4),
            ],
        );
        assert!(cache.needs_refresh(3, 2));
        let report = cache.refresh(&loaded);
        assert!(report.changed);
        assert_eq!(cache.revision(), 3);
        assert_eq!(cache.get("g1", "TWO_ONBOARDING_DRY_RUN"), None);
        assert_eq!(cache.get("g1", "TWO_AUTOMOD_REPEAT_COUNT"), Some(&json!(2)));
        assert_eq!(cache.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(&json!(9)));
    }

    #[test]
    fn cold_and_env_only_rows_are_partitioned_with_log_data() {
        let mut cache = SettingsCache::default();
        let loaded = snapshot(
            1,
            vec![
                row("g1", "TWO_FEED_POLL_SECONDS", json!(600), 1),
                row("g1", "TWO_MODERATION", json!("1"), 2),
                row("g1", "TWO_TOTALLY_MADE_UP", json!("1"), 3),
            ],
        );
        let report = cache.refresh(&loaded);
        assert!(report.changed);
        assert!(report.hot.is_empty());
        assert_eq!(report.cold.len(), 1);
        assert_eq!(report.cold[0].key, "TWO_FEED_POLL_SECONDS");
        assert_eq!(report.ignored.len(), 2);
        let reasons: Vec<_> = report
            .ignored
            .iter()
            .map(|i| (i.key.as_str(), i.reason))
            .collect();
        assert!(reasons.contains(&("TWO_MODERATION", IgnoreReason::EnvOnly)));
        assert!(reasons.contains(&("TWO_TOTALLY_MADE_UP", IgnoreReason::Unknown)));
        // Cold keys stay in the snapshot (restart applies them); env-only and
        // unknown keys never do, through either read API.
        assert_eq!(cache.get("g1", "TWO_FEED_POLL_SECONDS"), Some(&json!(600)));
        assert_eq!(cache.get("g1", "TWO_MODERATION"), None);
        assert_eq!(cache.get("g1", "TWO_TOTALLY_MADE_UP"), None);
        let rendered = cache.env_snapshot(Some("g1"));
        assert_eq!(
            rendered.get("TWO_FEED_POLL_SECONDS").map(String::as_str),
            Some("600")
        );
        assert!(!rendered.contains_key("TWO_MODERATION"));
        assert!(!rendered.contains_key("TWO_TOTALLY_MADE_UP"));
    }

    #[test]
    fn initial_load_getter_refuses_every_env_only_and_unknown_key() {
        let mut rows: Vec<_> = EXPECTED_ENV_ONLY
            .iter()
            .map(|key| row("g1", key, json!("fixture"), 1))
            .collect();
        rows.push(row("g1", "TWO_INTERNAL_FUTURE_GATE", json!("fixture"), 1));
        rows.push(row("g1", "TWO_UNKNOWN", json!("fixture"), 1));
        let cache = SettingsCache::load(&snapshot(1, rows.clone()));
        for row in rows {
            assert_eq!(cache.get("g1", &row.key), None, "{}", row.key);
        }
        assert!(cache.env_snapshot(Some("g1")).is_empty());
    }

    #[test]
    fn snapshots_are_per_guild() {
        let cache = SettingsCache::load(&snapshot(
            2,
            vec![
                row("g1", "TWO_ONBOARDING_DRY_RUN", json!(true), 1),
                row("g2", "TWO_ONBOARDING_DRY_RUN", json!(false), 2),
            ],
        ));
        assert_eq!(
            cache
                .env_snapshot(Some("g1"))
                .get("TWO_ONBOARDING_DRY_RUN")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            cache
                .env_snapshot(Some("g2"))
                .get("TWO_ONBOARDING_DRY_RUN")
                .map(String::as_str),
            Some("0")
        );
        assert!(cache.env_snapshot(None).is_empty());
    }

    #[test]
    fn write_validation_refuses_before_any_sql() {
        // Env-only refused.
        assert!(matches!(
            validate_write("g1", "TWO_MODERATION", Some(json!("1")), "admin"),
            Err(WriteRefusal::EnvOnly(_))
        ));
        // Unknown refused.
        assert!(matches!(
            validate_write("g1", "TWO_NEW_GATE", Some(json!("1")), "admin"),
            Err(WriteRefusal::Unknown(_))
        ));
        // Prefix-refused even though uncatalogued.
        assert!(matches!(
            validate_write(
                "g1",
                "TWO_INTERNAL_ALLOW_ANYTHING",
                Some(json!("1")),
                "admin"
            ),
            Err(WriteRefusal::EnvOnly(_))
        ));
        // Unattributed refused — even for a hot key.
        assert_eq!(
            validate_write("g1", "TWO_RAID_JOIN_THRESHOLD", Some(json!(3)), ""),
            Err(WriteRefusal::MissingActor)
        );
        // Hot and cold validate; delete validates too.
        let hot = validate_write("g1", "TWO_RAID_JOIN_THRESHOLD", Some(json!(3)), "admin")
            .expect("hot validates");
        assert_eq!(hot.class, SettingClass::Hot);
        assert_eq!(hot.action, WriteAction::Upsert(json!(3)));
        let cold = validate_write("g1", "TWO_FEED_POLL_SECONDS", Some(json!(600)), "admin")
            .expect("cold validates");
        assert_eq!(cold.class, SettingClass::Cold);
        let delete = validate_write("g1", "TWO_ONBOARDING_DRY_RUN", None, "admin")
            .expect("delete validates");
        assert_eq!(delete.action, WriteAction::Delete);
    }

    #[test]
    fn live_channel_publishes_only_when_the_poll_moved() {
        let (mut writer, live) = live_channel();
        // Parked or pre-first-poll readers see the empty revision-0 cache:
        // every lookup falls through to the environment.
        assert_eq!(live.revision(), 0);
        assert_eq!(live.get("g1", "TWO_RAID_JOIN_THRESHOLD"), None);
        assert!(live.env_snapshot(Some("g1")).is_empty());

        let loaded = snapshot(1, vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3), 1)]);
        assert!(writer.needs_refresh(1, 1));
        let report = writer.publish(&loaded);
        assert!(report.changed);
        assert_eq!(live.revision(), 1);
        assert_eq!(live.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(json!(3)));
        assert_eq!(writer.revision(), 1);

        // Republishing identical marks is a no-op for the cache; even an
        // identical-content publish under a moved revision keeps readers on
        // the same Arc unless the diff found a real change.
        let published = live.snapshot();
        assert!(!writer.needs_refresh(1, 1));
        let report = writer.publish(&snapshot(
            1,
            vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3), 1)],
        ));
        assert!(!report.changed);
        assert!(Arc::ptr_eq(&published, &live.snapshot()));
    }

    #[test]
    fn live_channel_delete_hands_the_key_back_to_the_environment() {
        let (mut writer, live) = live_channel();
        writer.publish(&snapshot(
            1,
            vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3), 1)],
        ));
        assert_eq!(live.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(json!(3)));

        let report = writer.publish(&snapshot(2, vec![]));
        assert!(report.changed);
        assert_eq!(live.revision(), 2);
        assert_eq!(live.get("g1", "TWO_RAID_JOIN_THRESHOLD"), None);
        assert!(live.env_snapshot(Some("g1")).is_empty());
    }
}
