//! Guild-settings hot reload: catalogue, cache, and poll decisions.
//!
//! Ports `src/core/settingsCatalog.ts` (the ~90-key…117-key `env_only`/`cold`/
//! `hot` classification, TOG-3100) and the DB-free half of `src/core/settings.ts`
//! (`SettingsStore` cache + version poll + env rendering, TOG-3093 slice 1)
//! from legacy two-bot as framework-free data plus pure functions. Storage
//! stays behind the caller: the sqlx store lives in `two-bot-cutover`
//! (`crates/cutover/src/settings.rs`, migrations `0330`–`0331`), which calls
//! into this module; the 15 s ticker wires into the bot runtime in a follow-up
//! once the S4 router/REST slices land — this module builds no dispatcher and
//! no HTTP client.
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

use std::collections::HashMap;

use serde_json::Value;

/// How often the version poll runs (legacy `pollSeconds ?? 15`, TOG-3093
/// ADR §2.1). A poll is one cheap `max(version)` + `count(*)` query; NOTIFY
/// would need a dedicated connection out of a pool sized at 5 on purpose.
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
        Value::Number(n) => Some(n.to_string()),
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

/// In-memory cache over `guild_settings`, refreshed by the version poll
/// (legacy `SettingsStore` minus the database: reads are synchronous and never
/// touch the DB because the callers are gateway handlers on a hot path;
/// staleness is bounded by [`POLL_SECONDS`]).
#[derive(Debug, Clone, Default)]
pub struct SettingsCache {
    entries: HashMap<(String, String), Value>,
    /// Highest `version` the cache has seen. `0` means "nothing loaded yet".
    version: i64,
    /// Rows the cache was built from. Half of the change detector, not a
    /// statistic: `max(version)` alone cannot see a delete, because the
    /// version lived in the deleted row — unless the deleted row held the
    /// maximum, removing it leaves the maximum exactly where it was.
    row_count: usize,
}

impl SettingsCache {
    /// Fill (or refill) the cache from freshly loaded rows. Call once before
    /// reading, and again on every poll that moved.
    #[must_use]
    pub fn load(rows: &[SettingRow]) -> Self {
        let mut cache = Self::default();
        cache.rebuild(rows);
        cache
    }

    fn rebuild(&mut self, rows: &[SettingRow]) {
        let mut entries = HashMap::with_capacity(rows.len());
        let mut max = 0i64;
        for row in rows {
            entries.insert((row.guild_id.clone(), row.key.clone()), row.value.clone());
            if row.version > max {
                max = row.version;
            }
        }
        self.entries = entries;
        self.version = max;
        self.row_count = rows.len();
    }

    /// Highest version seen (`0` before the first load).
    #[must_use]
    pub fn version(&self) -> i64 {
        self.version
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

    /// One cheap comparison, evaluated from the poll's `max(version)` and
    /// `count(*)`: a refetch is needed when either moved. `!=` rather than `>`
    /// so a restored backup with a lower sequence reloads too.
    #[must_use]
    pub fn needs_refresh(&self, latest_version: i64, latest_count: i64) -> bool {
        latest_version != self.version || latest_count != self.row_count as i64
    }

    /// Rebuild the cache from freshly loaded rows and report what moved,
    /// partitioned by class: hot changes apply live, cold changes are stored
    /// but need a restart, env-only/unknown rows are ignored with a log line
    /// (legacy `refreshIfChanged` + the `HOT_WIRED_FIELDS` change log).
    pub fn refresh(&mut self, rows: &[SettingRow]) -> RefreshReport {
        let from_version = self.version;
        let from_rows = self.row_count;

        // `new` borrows `rows`; deletions read `self.entries` directly, so no
        // intermediate borrowed-key map is needed.
        let mut new: HashMap<(String, String), &Value> = HashMap::with_capacity(rows.len());
        for row in rows {
            new.insert((row.guild_id.clone(), row.key.clone()), &row.value);
        }

        let mut report = RefreshReport {
            from_version,
            from_rows,
            to_version: 0,
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

        self.rebuild(rows);
        report.to_version = self.version;
        report.changed = self.version != from_version || self.row_count != from_rows;
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
    pub from_version: i64,
    pub to_version: i64,
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

    fn row(guild: &str, key: &str, value: Value, version: i64) -> SettingRow {
        SettingRow {
            guild_id: guild.to_owned(),
            key: key.to_owned(),
            value,
            version,
        }
    }

    #[test]
    fn hot_change_is_visible_within_one_poll() {
        let mut cache = SettingsCache::load(&[]);
        assert!(!cache.needs_refresh(0, 0));
        let rows = vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(3), 1)];
        assert!(cache.needs_refresh(1, 1));
        let report = cache.refresh(&rows);
        assert!(report.changed);
        assert_eq!(cache.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(&json!(3)));
        assert_eq!(report.hot.len(), 1);
        assert_eq!(report.hot[0].key, "TWO_RAID_JOIN_THRESHOLD");
        assert_eq!(report.hot[0].new.as_deref(), Some("3"));
        assert!(report.cold.is_empty() && report.ignored.is_empty());
        assert!(!cache.needs_refresh(1, 1), "second poll is a no-op");
    }

    #[test]
    fn delete_is_visible_even_when_it_is_not_the_newest_row() {
        // The staging-found hole (TOG-3100): the deleted row carries its
        // version away, so max(version) alone never moves when a newer row
        // survives. The row count is what catches it.
        let rows = vec![
            row("g1", "TWO_ONBOARDING_DRY_RUN", json!(true), 1),
            row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(9), 2),
        ];
        let mut cache = SettingsCache::load(&rows);
        let version_before = cache.version();
        let remaining = vec![row("g1", "TWO_RAID_JOIN_THRESHOLD", json!(9), 2)];
        assert!(
            cache.needs_refresh(version_before, 1),
            "count moved while max(version) stayed put"
        );
        let report = cache.refresh(&remaining);
        assert!(report.changed);
        assert_eq!(cache.version(), version_before);
        assert_eq!(cache.get("g1", "TWO_ONBOARDING_DRY_RUN"), None);
        assert_eq!(cache.get("g1", "TWO_RAID_JOIN_THRESHOLD"), Some(&json!(9)));
    }

    #[test]
    fn cold_and_env_only_rows_are_partitioned_with_log_data() {
        let mut cache = SettingsCache::load(&[]);
        let rows = vec![
            row("g1", "TWO_FEED_POLL_SECONDS", json!(600), 1),
            row("g1", "TWO_MODERATION", json!("1"), 2),
            row("g1", "TWO_TOTALLY_MADE_UP", json!("1"), 3),
        ];
        let report = cache.refresh(&rows);
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
        // unknown keys never do.
        let snapshot = cache.env_snapshot(Some("g1"));
        assert_eq!(
            snapshot.get("TWO_FEED_POLL_SECONDS").map(String::as_str),
            Some("600")
        );
        assert!(!snapshot.contains_key("TWO_MODERATION"));
        assert!(!snapshot.contains_key("TWO_TOTALLY_MADE_UP"));
    }

    #[test]
    fn snapshots_are_per_guild() {
        let cache = SettingsCache::load(&[
            row("g1", "TWO_ONBOARDING_DRY_RUN", json!(true), 1),
            row("g2", "TWO_ONBOARDING_DRY_RUN", json!(false), 2),
        ]);
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
}
