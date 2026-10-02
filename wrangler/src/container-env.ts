/**
 * Reviewed `TWO_*` runtime flags the Worker forwards into the Container
 * (TOG-12020). Before this list, `containerEnvVars` forwarded only the token,
 * database, guild and listener, so every feature gate fell back to its
 * default under the Worker (docs/cutover.md "Runtime gates").
 *
 * Explicit names only — never a prefix wildcard. A name joins
 * FORWARDED_FLAGS when it is a non-secret runtime flag the bot reads from
 * its process environment. Everything else the Rust crates mention goes in
 * NOT_FORWARDED with a reason; test/container-env.test.ts fails when a
 * `TWO_*` name in crates/ is in neither list. Classification reference:
 * `crates/core/src/settings.rs` (`SETTING_CLASSES`, `is_env_only_key`,
 * `is_declared_env_only`) — env-only there means "never from the database",
 * not "secret": reach bounds such as TWO_OWEN_USER_ID must arrive via env.
 *
 * Secrets never pass through here. A secret the Container needs gets its own
 * explicit line in `containerEnvVars`, like DISCORD_TOKEN and DATABASE_URL.
 */

export const FORWARDED_FLAGS = [
  // Feature gates read at gateway boot (FeatureGates, ModerationGates).
  "TWO_ANNOUNCEMENTS",
  "TWO_AUTOMATIONS",
  "TWO_TEXT_COMMANDS",
  "TWO_FEED_POLL_SECONDS",
  "TWO_MODERATION",
  "TWO_MODERATION_PROTECTED_ROLE_IDS",
  "TWO_OWEN_USER_ID",
  // Automod (gateway intents + AutomodConfig).
  "TWO_AUTOMOD",
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
  // Onboarding and self-roles.
  "TWO_ONBOARDING_MODE",
  "TWO_ONBOARDING_DRY_RUN",
  "TWO_SELF_ROLE_PANELS",
  "TWO_SELF_ROLE_DRY_RUN",
  // Guild command registry drift CLI and opt-in boot publish (TOG-10860).
  "TWO_COMMANDS_PUBLISH_ON_BOOT",
  "TWO_COMMANDS_ALLOW_LIVE_GUILD",
  // Community scorecard, classifier and jobs.
  "TWO_COMMUNITY_SCORECARD",
  "TWO_COMMUNITY_RECOMMENDATIONS",
  "TWO_COMMUNITY_CORRECTION_CYCLES",
  "TWO_COMMUNITY_CLASSIFIER_VERSION",
  "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS",
  "TWO_COMMUNITY_RAID_ACTOR_IDS",
  "TWO_COMMUNITY_STAGING_ACTOR_IDS",
  "TWO_COMMUNITY_STAGING_GUILD_IDS",
  "TWO_COMMUNITY_TEST_ACTOR_IDS",
  "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
  "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
  "TWO_INACTIVITY_DAYS",
  "TWO_PRESENCE_PROBE",
  "TWO_TICKET_COOLDOWN_SECONDS",
  // Raid watch, join risk and anti-nuke.
  "TWO_RAID_JOIN_THRESHOLD",
  "TWO_RAID_WINDOW_SECONDS",
  "TWO_JOIN_RISK_THRESHOLD",
  "TWO_JOIN_RISK_WINDOW_SECONDS",
  "TWO_BULK_JOIN_WINDOW_UNTIL",
  "TWO_ANTI_NUKE",
  "TWO_ANTI_NUKE_DRY_RUN",
  "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS",
  "TWO_ANTI_NUKE_HEAT_THRESHOLD",
  "TWO_ANTI_NUKE_WINDOW_SECONDS",
  "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
  "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
  // Temporary voice rooms.
  "TWO_VOICE",
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
  // Template assistant (V12): non-secret endpoint URL + model name; the
  // endpoint credential (if any) travels as its own Container secret, never
  // through this flag allowlist.
  "TWO_ASSISTANT_ENDPOINT",
  "TWO_ASSISTANT_MODEL",
] as const;

export type ForwardedFlag = (typeof FORWARDED_FLAGS)[number];

/** Optional Worker vars, one per forwarded flag (mixed into `Env`). */
export type ForwardedFlagEnv = { [K in ForwardedFlag]?: string };

const SECRET = "secret; a Container secret needs its own explicit containerEnvVars line";
const TEST = "test-only: CI service containers or a test fixture name";
const BACKUP = "host backup/restore timer input (deploy/*.service), not the Container";
const BIND = "network bind; the Worker sets LISTEN_ADDR and proxies only /health and /readyz";
const CAPABILITY =
  "TWO_INTERNAL_* website-to-bot capability gate; no Container reader, widening needs its own reviewed card";
const LEGACY = "legacy input with no Next reader; settings.rs keeps it only to refuse storage";
const REDIRECT = "go.two.gg redirect is served by the Worker (REDIRECT_*), not the Container";

/** Every other `TWO_*` name in crates/, with why it stays out of the Container. */
export const NOT_FORWARDED: Readonly<Record<string, string>> = {
  TWO_DATABASE_URL: "cutover/operator CLI secret; the Container reads DATABASE_URL",
  TWO_STAGING_DATABASE_URL: SECRET,
  TWO_BOT_STAGING_MIGRATOR_DATABASE_URL: SECRET,
  TWO_RESTORE_URL: SECRET,
  TWO_MODERATION_AUDIT_SECRET: SECRET,
  TWO_ONBOARDING_ROTA_PSEUDONYM_KEY: SECRET,
  TWO_BACKUP_S3_ACCESS_KEY_ID: SECRET,
  TWO_BACKUP_S3_SECRET_ACCESS_KEY: SECRET,
  TWO_DB_POOL_MAX: "cutover CLI pool size, not read by the Container runtime",
  TWO_DATABASE_TLS: "cutover/operator CLI TLS policy for database_tls::enforce (default required); not read by the Container runtime",
  TWO_ERASURE_ACTOR: "erase-member operator CLI audit actor, set per invocation; not read by the Container runtime",
  TWO_GUILD_CONFIG_OFFLINE_TEST: "guild-config backup CLI offline-fixture gate, set only by CLI tests; not read by the Container runtime",
  TWO_BACKUP_DIR: BACKUP,
  TWO_BACKUP_KEEP: BACKUP,
  TWO_BACKUP_S3_BUCKET: BACKUP,
  TWO_BACKUP_S3_ENDPOINT: BACKUP,
  TWO_BACKUP_S3_PREFIX: BACKUP,
  TWO_BACKUP_S3_REGION: BACKUP,
  TWO_BACKUP_S3_TIMEOUT_MS: BACKUP,
  TWO_BACKUP_UPLOAD_CMD: BACKUP,
  TWO_GUILD_CONFIG_BACKUP_DIR: BACKUP,
  TWO_GUILD_CONFIG_UPLOAD_CMD: BACKUP,
  TWO_ANTI_NUKE_SNAPSHOT_PATH: "filesystem write path; Container disk is ephemeral",
  TWO_HEALTH_BIND_HOST: BIND,
  TWO_HEALTH_PORT: BIND,
  TWO_INTERNAL_BIND_HOST: BIND,
  TWO_INTERNAL_PORT: BIND,
  TWO_REDIRECT_BIND_HOST: BIND,
  TWO_REDIRECT_PORT: BIND,
  TWO_REDIRECT_FALLBACK_CODE: REDIRECT,
  TWO_REDIRECT_TRUSTED_PROXIES: REDIRECT,
  TWO_INTERNAL_ACTIONS: CAPABILITY,
  TWO_INTERNAL_ALLOW_ADD_MEMBER: CAPABILITY,
  TWO_INTERNAL_ALLOW_AUTOMATIONS: CAPABILITY,
  TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE: CAPABILITY,
  TWO_INTERNAL_ALLOW_EVENT_CANCEL: CAPABILITY,
  TWO_INTERNAL_ALLOW_EVENT_READ: CAPABILITY,
  TWO_INTERNAL_ALLOW_MODERATION: CAPABILITY,
  TWO_INTERNAL_ALLOW_SETTINGS: CAPABILITY,
  TWO_INTERNAL_CHANNEL_KEYS: CAPABILITY,
  TWO_INTERNAL_KEYS: CAPABILITY,
  TWO_INTERNAL_ROLE_KEYS: CAPABILITY,
  TWO_ONBOARDING_ROTA_MEASUREMENT: LEGACY,
  TWO_ONBOARDING_ROTA_NOTICE: LEGACY,
  TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID: LEGACY,
  TWO_ONBOARDING_ROTA_READER_IDS: LEGACY,
  TWO_STAGING_RESTART_CONTAINMENT: LEGACY,
  TWO_STAGING_RESTART_SYNTHETIC_ACTORS: LEGACY,
  TWO_TEST_DATABASE_URL: TEST,
  TWO_BOT_TEST_DATABASE_URL: TEST,
  TWO_GATEWAY_TEST_DATABASE_URL: TEST,
  TWO_ROLES_TEST_DATABASE_URL: TEST,
  TWO_BOT_TEST_BACKUP_BIN: TEST,
  TWO_LEVELING_TEST_CI: TEST,
  TWO_LFG_TESTDB_CI: TEST,
  TWO_TEST_COUNTER_INTERVAL_MS: TEST,
  TWO_TEST_EVENTS_INTERVAL_MS: TEST,
  TWO_TEST_RANK_INTERVAL_MS: TEST,
  TWO_FEED_XML_RESOURCE_PROBE: TEST,
  TWO_INTERNAL_ALLOW_ANYTHING: TEST,
  TWO_INTERNAL_FUTURE_GATE: TEST,
  TWO_INTERNAL_NEW_GATE: TEST,
  TWO_INTERNALISED_GREETING: TEST,
  TWO_NEW_GATE: TEST,
  TWO_NEVER_EXISTED: TEST,
  TWO_SOMETHING_NEW: TEST,
  TWO_TOTALLY_MADE_UP: TEST,
  TWO_TYPO_KEYS: TEST,
  TWO_UNKNOWN: TEST,
};

/**
 * Copy the allowlisted flags that the Worker env defines as strings,
 * verbatim (an empty string is forwarded: set-but-empty is not unset to the
 * Rust readers). Unlisted names and non-string bindings are dropped.
 */
export function forwardedFlagVars(env: object): Record<string, string> {
  const source = env as Readonly<Record<string, unknown>>;
  const vars: Record<string, string> = {};
  for (const name of FORWARDED_FLAGS) {
    const value = source[name];
    if (typeof value === "string") vars[name] = value;
  }
  return vars;
}
