//! Community scorecard domain: classifier, weekly build, and Monday schedule.
//!
//! Ports the schedule-relevant core of `src/analytics/communityScorecard.ts` +
//! `src/analytics/communityClassifier.ts` + `src/analytics/communityFacts.ts`
//! (fact-type/classification vocabularies only) + `src/jobs/communityScorecard.ts`
//! from legacy two-bot (frozen `main` @ `d5d11793`) as framework-free data plus
//! pure functions, in the same style as `leveling.rs`/`moderation.rs`. No
//! Discord client, no SQL here: the caller supplies fact rows and coverage
//! state, the builder returns a plain-data outcome, and the store persists it
//! — so the weekly math unit-tests without Discord or Postgres, including an
//! exact-output check against a legacy-produced fixture.
//!
//! Raw facts are content-minimized and append-only. Scorecard revisions and
//! alert dedupe rows stay auditable; no recommendation path may delete source
//! facts. Rota extensions stay dropped (parity §9 drop 1).
//!
//! Source files (legacy `two-bot`):
//! - `src/analytics/communityScorecard.ts` (`buildCommunityScorecard`,
//!   `previousClosedCommunityWeek`, intervention selection, `INTERVENTION_CODES`)
//! - `src/analytics/communityClassifier.ts` (precedence, env config)
//! - `src/analytics/communityFacts.ts` (`COMMUNITY_FACT_TYPES`, idempotency shapes)
//! - `src/jobs/communityScorecard.ts` (`isCommunityScorecardRunTime`, 60s tick,
//!   exactly-once guard)
//! - `migrations/0018_community_scorecard.sql` (tables, documented in 0311)

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::funnel::{format_iso_millis, parse_iso_millis};

/// Fact streams, in legacy heartbeat order (legacy `COMMUNITY_FACT_TYPES`).
pub const COMMUNITY_FACT_TYPES: [&str; 6] = [
    "message_created",
    "voice_session_started",
    "voice_session_ended",
    "member_joined",
    "event_attended",
    "rules_accepted",
];

/// Streams with a live production writer: the only streams `scorecard_once`
/// may mark covered. That is `event_attended` (host check-in via
/// `rsvp_store::record_checkin`) and `message_created` (gateway
/// `MessageCreate` via `community_store::record_message_fact`); voice, join
/// and rules capture land in later slices, one per stream. This must always
/// equal the `Some` rows of [`STREAM_WRITERS`]; the
/// `captured_streams_match_live_writers` guard fails otherwise.
pub const CAPTURED_STREAMS: [&str; 2] = ["event_attended", "message_created"];

/// Capture registry: every fact stream with the production writer that appends
/// it, or `None` while no live path writes it. Single source of truth for both
/// directions of the guard: a stream in [`CAPTURED_STREAMS`] whose row is
/// `None` has no writer, and a `Some` row missing from [`CAPTURED_STREAMS`]
/// is a writer the scorecard does not claim. Landing a writer flips its row
/// and grows [`CAPTURED_STREAMS`] in the same PR.
pub const STREAM_WRITERS: [(&str, Option<&str>); 6] = [
    (
        "message_created",
        Some("community_store::record_message_fact"),
    ),
    ("voice_session_started", None),
    ("voice_session_ended", None),
    ("member_joined", None),
    ("event_attended", Some("rsvp_store::record_checkin")),
    ("rules_accepted", None),
];

/// Exclusion buckets (legacy `COMMUNITY_CLASSIFICATIONS`). Contract
/// precedence is deliberate: a Discord bot posting through a webhook is a bot
/// bucket, not two exclusions, so reconciliation remains exact.
pub const COMMUNITY_CLASSIFICATIONS: [&str; 7] = [
    "eligible_human",
    "bot",
    "webhook",
    "staff_automation",
    "raid",
    "staging",
    "test",
];

/// Catch-all bucket for rows whose stored classification is not in
/// [`COMMUNITY_CLASSIFICATIONS`] (legacy `'invalid/unknown'`).
pub const INVALID_CLASSIFICATION: &str = "invalid/unknown";

/// 60s scheduler tick (legacy `intervalMs ?? 60_000`): the tick is cheap and
/// the Monday 06:15 UTC fire is exact; the per-Monday guard makes it fire once.
pub const SCORECARD_TICK_INTERVAL_MS: u64 = 60_000;
/// Monday (legacy `MONDAY = 1`, `getUTCDay` Sunday-zero).
pub const RUN_WEEKDAY_MONDAY: u32 = 1;
/// 06:15 UTC (legacy `RUN_HOUR_UTC = 6`, `RUN_MINUTE_UTC = 15`).
pub const RUN_HOUR_UTC: u32 = 6;
/// First minute of the fire window; the predicate holds for the rest of hour 6.
pub const RUN_MINUTE_UTC: u32 = 15;

/// Bot-noise alert line (legacy `BOT_NOISE_THRESHOLD = 0.2`): automated posts
/// at or above 20% of human-space messages alert.
pub const BOT_NOISE_THRESHOLD: f64 = 0.2;
/// First-reply breach line (legacy `FIRST_REPLY_BREACH_MS = DAY_MS`).
pub const FIRST_REPLY_BREACH_MS: i64 = 86_400_000;
/// Evidence line (legacy: `humans.size < 5` is insufficient).
pub const MIN_EVIDENCE_HUMANS: usize = 5;
/// Weekly-active voice line: 600 deduplicated voice seconds counts as active.
pub const VOICE_ACTIVE_SECONDS: f64 = 600.0;

/// Intervention codes (legacy `COMMUNITY_INTERVENTION_CODES`, kept verbatim —
/// the runs-table CHECK and legacy fixtures name all seven). `EVENT_AT_RISK`
/// and `CORE_DECLINE` are reserved: the selector below never emits them, same
/// as legacy `selectIntervention`.
pub const COMMUNITY_INTERVENTION_CODES: [&str; 7] = [
    "INGESTION_INCOMPLETE",
    "BOT_NOISE_HIGH",
    "FIRST_REPLY_BREACH",
    "EVENT_AT_RISK",
    "CORE_DECLINE",
    "HOLD",
    "none_insufficient_evidence",
];

/// Classifier tuning (legacy `CommunityClassifierConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierConfig {
    pub version: String,
    pub automation_actor_ids: HashSet<String>,
    pub raid_actor_ids: HashSet<String>,
    pub staging_guild_ids: HashSet<String>,
    pub staging_actor_ids: HashSet<String>,
    pub test_actor_ids: HashSet<String>,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            version: "community-v1".to_owned(),
            automation_actor_ids: HashSet::new(),
            raid_actor_ids: HashSet::new(),
            staging_guild_ids: HashSet::new(),
            staging_actor_ids: HashSet::new(),
            test_actor_ids: HashSet::new(),
        }
    }
}

/// Invalid scorecard-gate environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScorecardGateError {
    #[error("TWO_COMMUNITY_CORRECTION_CYCLES must be a non-negative integer, got {0:?}")]
    InvalidCorrectionCycles(String),
}

/// Scorecard env gates (legacy `src/core/config.ts` + `settingsCatalog.ts`):
/// `TWO_COMMUNITY_SCORECARD=1` enables, `TWO_COMMUNITY_RECOMMENDATIONS=0`
/// disables, `TWO_COMMUNITY_CORRECTION_CYCLES` counts operator corrections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScorecardGates {
    /// `TWO_COMMUNITY_SCORECARD=1` — record facts and run the Monday job.
    pub enabled: bool,
    /// `TWO_COMMUNITY_RECOMMENDATIONS` — on unless explicitly `0`.
    pub recommendations_enabled: bool,
    /// `TWO_COMMUNITY_CORRECTION_CYCLES` — default 0.
    pub correction_cycles: u32,
}

impl ScorecardGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, ScorecardGateError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ScorecardGateError> {
        let correction_cycles = match vars.get("TWO_COMMUNITY_CORRECTION_CYCLES") {
            None => 0,
            Some(raw) => raw
                .parse::<u32>()
                .map_err(|_| ScorecardGateError::InvalidCorrectionCycles(raw.clone()))?,
        };
        Ok(Self {
            enabled: vars
                .get("TWO_COMMUNITY_SCORECARD")
                .is_some_and(|v| v == "1"),
            recommendations_enabled: vars
                .get("TWO_COMMUNITY_RECOMMENDATIONS")
                .is_none_or(|v| v != "0"),
            correction_cycles,
        })
    }
}

fn id_set(vars: &HashMap<String, String>, key: &str) -> HashSet<String> {
    vars.get(key)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl ClassifierConfig {
    /// Read classifier config from the process environment (legacy
    /// `loadCommunityClassifierConfig`).
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read classifier config from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        Self {
            version: vars
                .get("TWO_COMMUNITY_CLASSIFIER_VERSION")
                .cloned()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "community-v1".to_owned()),
            automation_actor_ids: id_set(vars, "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS"),
            raid_actor_ids: id_set(vars, "TWO_COMMUNITY_RAID_ACTOR_IDS"),
            staging_guild_ids: id_set(vars, "TWO_COMMUNITY_STAGING_GUILD_IDS"),
            staging_actor_ids: id_set(vars, "TWO_COMMUNITY_STAGING_ACTOR_IDS"),
            test_actor_ids: id_set(vars, "TWO_COMMUNITY_TEST_ACTOR_IDS"),
        }
    }
}

/// Classifier input (legacy `CommunityClassifierInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyInput {
    pub guild_id: String,
    pub actor_id: String,
    pub is_bot: bool,
    pub webhook_id: Option<String>,
    pub is_staff_automation: bool,
    pub is_raid: bool,
    pub is_staging: bool,
    pub is_test: bool,
}

/// Classification verdict (legacy `CommunityClassificationResult`). Owned
/// strings: verdicts are built both by [`classify`] (static rules) and by
/// tests/fixtures (dynamic names), so borrowing would force leaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub classification: String,
    pub classifier_version: String,
    pub matched_rule: String,
}

/// Classify one actor (legacy `CommunityClassifier.classify`): precedence is
/// bot → webhook → staff automation → raid → staging → test → eligible human.
#[must_use]
pub fn classify(config: &ClassifierConfig, input: &ClassifyInput) -> Classification {
    let verdict = |classification: &str, matched_rule: &str| Classification {
        classification: classification.to_owned(),
        classifier_version: config.version.clone(),
        matched_rule: matched_rule.to_owned(),
    };
    if input.is_bot {
        return verdict("bot", "discord_bot");
    }
    if input.webhook_id.as_deref().is_some_and(|w| !w.is_empty()) {
        return verdict("webhook", "discord_webhook");
    }
    if input.is_staff_automation || config.automation_actor_ids.contains(&input.actor_id) {
        return verdict("staff_automation", "configured_automation_actor");
    }
    if input.is_raid || config.raid_actor_ids.contains(&input.actor_id) {
        return verdict("raid", "configured_raid_actor");
    }
    if input.is_staging
        || config.staging_guild_ids.contains(&input.guild_id)
        || config.staging_actor_ids.contains(&input.actor_id)
    {
        return verdict("staging", "configured_staging_scope");
    }
    if input.is_test || config.test_actor_ids.contains(&input.actor_id) {
        return verdict("test", "configured_test_actor");
    }
    verdict("eligible_human", "no_exclusion_matched")
}

/// One fact row as the builder consumes it (legacy `CommunityFactRow` +
/// `ParsedFact`): already scoped by the store to one guild, one closed week,
/// and `id <= watermark`, ordered by `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactRow {
    pub id: i64,
    pub guild_id: String,
    pub event_type: String,
    pub source_event_id: String,
    pub actor_id: Option<String>,
    pub occurred_at: String,
    pub source: String,
    pub classifier_version: String,
    pub classification: String,
    pub matched_rule: String,
    pub metadata: Option<String>,
    pub idempotency_key: String,
}

/// One stream heartbeat (legacy `community_stream_heartbeats` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCoverage {
    pub stream: String,
    pub covered_from: String,
    pub covered_through: String,
}

/// Builder inputs (legacy `CommunityScorecardConfig` plus the coverage facts
/// the store gathers): everything `build_scorecard` needs, nothing it fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScorecardInputs {
    pub guild_id: String,
    pub classifier_version: String,
    pub week_start: String,
    pub week_end: String,
    pub watermark: i64,
    pub generated_at: String,
    pub recommendations_enabled: bool,
    pub correction_cycles: u32,
    pub facts: Vec<FactRow>,
    pub coverage: Vec<StreamCoverage>,
    /// The store's duplicate-`source_event_id` probe
    /// (`GROUP BY source_event_id, event_type HAVING COUNT(*) > 1`).
    pub duplicate_source_ids: bool,
    /// `COUNT(*)` of existing runs for `(guild, week_start, classifier_version)`.
    pub existing_revision_count: u64,
    /// Previously persisted scorecard JSON for the idempotency key, if any.
    pub existing_scorecard_json: Option<String>,
}

/// Malformed builder *configuration* (week bounds). Fact-level garbage stays
/// NaN-tolerant like legacy `Date.parse` — it is data, not a config error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScorecardBuildError {
    #[error("week bounds must be ISO-8601 UTC timestamps")]
    BadWeekBounds,
}

/// Reconciliation bucket (legacy `ReconciliationBucket`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReconciliationBucket {
    pub raw: u64,
    pub eligible_human: u64,
    pub bot: u64,
    pub webhook: u64,
    pub staff_automation: u64,
    pub raid: u64,
    pub staging: u64,
    pub test: u64,
    #[serde(rename = "invalid/unknown")]
    pub invalid_unknown: u64,
    pub reconciles: bool,
}

/// Join-source split (legacy `joinSources`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinSources {
    pub known: u64,
    pub unknown: u64,
}

/// Attendance rollup (legacy `eventAttendance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventAttendance {
    pub participations: usize,
    pub distinct_humans: usize,
}

/// Bot-noise gauge (legacy `botNoise`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotNoise {
    pub numerator: u64,
    pub denominator: u64,
    pub ratio: Option<f64>,
    pub alert: bool,
}

/// First-human-reply clock (legacy `firstHumanReply`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstHumanReply {
    pub median_seconds: Option<f64>,
    pub resolved_count: usize,
    pub eligible_join_count: usize,
    #[serde(rename = "noReplyWithin24hCount")]
    pub no_reply_within_24h_count: usize,
    pub pending_count: usize,
}

/// Selected intervention (legacy `intervention`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Intervention {
    pub code: String,
    pub reason: String,
}

/// The weekly scorecard (legacy `CommunityScorecard`, camelCase JSON keys so
/// serialized output matches the legacy fixture byte-for-shape).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scorecard {
    pub guild_id: String,
    pub week_start: String,
    pub week_end: String,
    pub generated_at: String,
    pub classifier_version: String,
    pub watermark: i64,
    pub idempotency_key: String,
    pub revision: u64,
    pub coverage_state: String,
    pub evidence_state: String,
    pub raw_fact_count: usize,
    pub weekly_active_humans: Option<usize>,
    pub human_messages: Option<u64>,
    pub eligible_joins: Option<u64>,
    pub join_sources: Option<JoinSources>,
    pub event_attendance: Option<EventAttendance>,
    pub bot_noise: Option<BotNoise>,
    pub first_human_reply: Option<FirstHumanReply>,
    pub exclusion_counts: BTreeMap<String, BTreeMap<String, u64>>,
    pub reconciliation: BTreeMap<String, ReconciliationBucket>,
    pub ingestion_errors: Vec<String>,
    pub intervention: Intervention,
    pub recommendations_enabled: bool,
    pub kill_switch_active: bool,
}

/// Builder outcome (legacy `CommunityScorecardResult`): `alert_key` is the
/// dedupe insert the store attempts (`None` emits no alert); the store reports
/// whether the insert won the race. Reused runs never alert.
#[derive(Debug, Clone, PartialEq)]
pub struct ScorecardOutcome {
    pub scorecard_json: String,
    pub reused: bool,
    pub alert_key: Option<String>,
    /// Store inputs echoed back (fresh builds only): parsed scorecard plus
    /// the `input_hash`/`input_count` the runs-table insert records.
    pub scorecard: Option<Scorecard>,
    pub input_hash: Option<String>,
    pub input_count: Option<usize>,
}

fn is_fact_type(value: &str) -> bool {
    COMMUNITY_FACT_TYPES.contains(&value)
}

fn normalize_classification(value: &str) -> &str {
    if COMMUNITY_CLASSIFICATIONS.contains(&value) {
        value
    } else {
        INVALID_CLASSIFICATION
    }
}

fn parse_metadata(value: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    value
        .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn meta_str(meta: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    match meta.get(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// Legacy `Number(metadata.durationSeconds)` coercion: explicit `null` is 0,
/// while a missing property is undefined/NaN and cannot count as voice activity.
fn voice_duration_seconds(meta: &serde_json::Map<String, serde_json::Value>) -> Option<f64> {
    match meta.get("durationSeconds") {
        None => None,
        Some(serde_json::Value::Null) => Some(0.0),
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.parse::<f64>().ok(),
        Some(_) => None,
    }
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input.as_bytes()))
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[i])
    } else {
        Some((sorted[i - 1] + sorted[i]) / 2.0)
    }
}

struct ParsedFact<'a> {
    row: &'a FactRow,
    classification: &'a str,
    meta: serde_json::Map<String, serde_json::Value>,
}

fn empty_bucket() -> ReconciliationBucket {
    ReconciliationBucket {
        raw: 0,
        eligible_human: 0,
        bot: 0,
        webhook: 0,
        staff_automation: 0,
        raid: 0,
        staging: 0,
        test: 0,
        invalid_unknown: 0,
        reconciles: true,
    }
}

fn bucket_add(bucket: &mut ReconciliationBucket, classification: &str) {
    match classification {
        "eligible_human" => bucket.eligible_human += 1,
        "bot" => bucket.bot += 1,
        "webhook" => bucket.webhook += 1,
        "staff_automation" => bucket.staff_automation += 1,
        "raid" => bucket.raid += 1,
        "staging" => bucket.staging += 1,
        "test" => bucket.test += 1,
        _ => bucket.invalid_unknown += 1,
    }
}

/// Monday 00:00 UTC of the week containing `ms` (legacy `weekStart`:
/// `(dow + 6) % 7` days back to Monday, Sunday-zero).
#[must_use]
pub fn week_start_ms(ms: i64) -> i64 {
    let days = ms.div_euclid(86_400_000);
    let dow_sunday_zero = (days + 4).rem_euclid(7);
    (days - (dow_sunday_zero + 6) % 7) * 86_400_000
}

/// UTC date `YYYY-MM-DD` of `ms`.
#[must_use]
pub fn format_date(ms: i64) -> String {
    format_iso_millis(ms)[..10].to_owned()
}

/// The closed week before the one containing `now_ms`: `[start, end)` where
/// `end` is this Monday 00:00 UTC (legacy `previousClosedCommunityWeek`).
#[must_use]
pub fn previous_closed_week(now_ms: i64) -> (String, String) {
    let end = week_start_ms(now_ms);
    let start = end - 7 * 86_400_000;
    (format_iso_millis(start), format_iso_millis(end))
}

/// Monday 06:15–06:59 UTC window (legacy `isCommunityScorecardRunTime`).
#[must_use]
pub fn is_scorecard_run_time(now_ms: i64) -> bool {
    let days = now_ms.div_euclid(86_400_000);
    let dow_sunday_zero = (days + 4).rem_euclid(7);
    let sod = now_ms.rem_euclid(86_400_000) / 1000;
    dow_sunday_zero == i64::from(RUN_WEEKDAY_MONDAY)
        && sod / 3600 == i64::from(RUN_HOUR_UTC)
        && (sod % 3600) / 60 >= i64::from(RUN_MINUTE_UTC)
}

/// One 60s-tick evaluation (legacy `startCommunityScorecardJob` tick): fire at
/// most once per Monday date — `last_attempted_week` is the `YYYY-MM-DD` the
/// job last attempted. Returns the week key to record when it fires.
#[must_use]
pub fn scorecard_tick(now_ms: i64, last_attempted_week: Option<&str>) -> Option<String> {
    if !is_scorecard_run_time(now_ms) {
        return None;
    }
    let key = format_date(now_ms);
    if last_attempted_week == Some(key.as_str()) {
        return None;
    }
    Some(key)
}

/// Build one weekly scorecard (legacy `buildCommunityScorecard`).
///
/// Facts arrive pre-scoped (guild + `[week_start, week_end)` + `id <=
/// watermark`, ordered by `id`); unknown event types are dropped, unknown
/// classifications land in `invalid/unknown`. Coverage, duplicate, revision
/// and reuse inputs come from the store. Rules under incomplete coverage fail
/// closed: human numerators stay `null` and the intervention is
/// `INGESTION_INCOMPLETE`.
pub fn build_scorecard(inputs: ScorecardInputs) -> Result<ScorecardOutcome, ScorecardBuildError> {
    let week_start_ms =
        parse_iso_millis(&inputs.week_start).ok_or(ScorecardBuildError::BadWeekBounds)?;
    let week_end_ms =
        parse_iso_millis(&inputs.week_end).ok_or(ScorecardBuildError::BadWeekBounds)?;
    let week_key = inputs.week_start[..10].to_owned();

    let facts: Vec<ParsedFact> = inputs
        .facts
        .iter()
        .filter(|f| is_fact_type(&f.event_type))
        .map(|f| ParsedFact {
            row: f,
            classification: normalize_classification(&f.classification),
            meta: parse_metadata(f.metadata.as_deref()),
        })
        .collect();

    let input_hash = sha256_hex(
        &facts
            .iter()
            .map(|f| format!("{}:{}", f.row.id, f.row.idempotency_key))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let idempotency_key = format!(
        "community-health:{}:{}:{}:{}",
        inputs.guild_id, week_key, inputs.classifier_version, inputs.watermark
    );

    if let Some(existing) = inputs.existing_scorecard_json {
        return Ok(ScorecardOutcome {
            scorecard_json: existing,
            reused: true,
            alert_key: None,
            scorecard: None,
            input_hash: None,
            input_count: None,
        });
    }

    // Coverage validation (legacy `validateCoverage`, error order preserved:
    // the fixture compares `ingestionErrors` element-wise).
    let mut ingestion_errors: Vec<String> = Vec::new();
    let covered: HashMap<&str, &StreamCoverage> = inputs
        .coverage
        .iter()
        .map(|c| (c.stream.as_str(), c))
        .collect();
    for stream in COMMUNITY_FACT_TYPES {
        match covered.get(stream) {
            Some(c)
                if c.covered_from <= inputs.week_start && c.covered_through >= inputs.week_end => {}
            _ => ingestion_errors.push(format!("missing_stream_coverage:{stream}")),
        }
    }
    if inputs.duplicate_source_ids {
        ingestion_errors.push("duplicate_source_ids".to_owned());
    }
    if facts
        .iter()
        .any(|f| f.classification == INVALID_CLASSIFICATION)
    {
        ingestion_errors.push("invalid_classification".to_owned());
    }
    if facts
        .iter()
        .any(|f| f.row.classifier_version != inputs.classifier_version)
    {
        ingestion_errors.push("classifier_version_mismatch".to_owned());
    }
    let scope_ok = facts.iter().all(|f| {
        f.row.guild_id == inputs.guild_id
            && f.row.occurred_at >= inputs.week_start
            && f.row.occurred_at < inputs.week_end
    });
    if !scope_ok {
        ingestion_errors.push("guild_or_week_scope_mismatch".to_owned());
    }
    let voice_ok = facts.iter().all(|f| {
        if f.row.event_type != "voice_session_ended" {
            return true;
        }
        // Legacy coverage exempts missing/null durations even though missing
        // durations are excluded by the voice-activity calculation.
        !f.meta.contains_key("durationSeconds")
            || f.meta.get("durationSeconds") == Some(&serde_json::Value::Null)
            || voice_duration_seconds(&f.meta).is_some_and(|d| d.is_finite() && d >= 0.0)
    });
    if !voice_ok {
        ingestion_errors.push("invalid_voice_duration".to_owned());
    }

    // Reconciliation (legacy `reconcile`): per-type rows plus `total`.
    let mut reconciliation: BTreeMap<String, ReconciliationBucket> = BTreeMap::new();
    for f in &facts {
        for key in [f.row.event_type.as_str(), "total"] {
            let bucket = reconciliation
                .entry(key.to_owned())
                .or_insert_with(empty_bucket);
            bucket.raw += 1;
            bucket_add(bucket, f.classification);
        }
    }
    for bucket in reconciliation.values_mut() {
        bucket.reconciles = bucket.raw
            == bucket.eligible_human
                + bucket.bot
                + bucket.webhook
                + bucket.staff_automation
                + bucket.raid
                + bucket.staging
                + bucket.test
                + bucket.invalid_unknown;
    }
    if reconciliation.values().any(|b| !b.reconciles) {
        ingestion_errors.push("reconciliation_failed".to_owned());
    }
    ingestion_errors.dedup();

    let coverage_state = if ingestion_errors.is_empty() {
        "complete"
    } else {
        "incomplete"
    };

    // Exclusion census (legacy `exclusionCounts`): every type × every bucket,
    // zeros included.
    let mut exclusion_counts: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for t in COMMUNITY_FACT_TYPES {
        let mut row = BTreeMap::new();
        for c in COMMUNITY_CLASSIFICATIONS {
            row.insert(c.to_owned(), 0);
        }
        row.insert(INVALID_CLASSIFICATION.to_owned(), 0);
        exclusion_counts.insert(t.to_owned(), row);
    }
    for f in &facts {
        if let Some(row) = exclusion_counts.get_mut(f.row.event_type.as_str()) {
            *row.entry(f.classification.to_owned()).or_insert(0) += 1;
        }
    }

    let eligible: Vec<&ParsedFact> = facts
        .iter()
        .filter(|f| f.classification == "eligible_human")
        .collect();

    // Deduplicated voice seconds per actor (legacy `unionVoiceSeconds`):
    // encounter order preserved (insertion-ordered merge, no float-order drift).
    let mut voice_intervals: Vec<(String, Vec<(i64, i64)>)> = Vec::new();
    for f in &facts {
        if f.row.event_type != "voice_session_ended"
            || f.classification != "eligible_human"
            || f.row.actor_id.as_deref().is_none_or(str::is_empty)
        {
            continue;
        }
        let start = meta_str(&f.meta, "startedAt").and_then(|s| parse_iso_millis(&s));
        let end = parse_iso_millis(&f.row.occurred_at);
        // `Number(null) === 0`: explicit null passes the finiteness checks
        // and the interval still merges; missing/NaN/non-finite/negative
        // durations are dropped.
        let duration = voice_duration_seconds(&f.meta);
        let (Some(start), Some(end), Some(duration)) = (start, end, duration) else {
            continue;
        };
        if !duration.is_finite() || duration < 0.0 {
            continue;
        }
        let a = start.max(week_start_ms);
        let b = end.min(week_end_ms);
        if b <= a {
            continue;
        }
        let actor = f.row.actor_id.clone().unwrap_or_default();
        match voice_intervals.iter_mut().find(|(a, _)| *a == actor) {
            Some((_, list)) => list.push((a, b)),
            None => voice_intervals.push((actor, vec![(a, b)])),
        }
    }
    let mut voice_seconds: Vec<(String, f64)> = Vec::new();
    for (actor, mut intervals) in voice_intervals {
        intervals.sort();
        let mut seconds = 0.0;
        let (mut open_start, mut open_end) = intervals[0];
        for (start, end) in intervals.into_iter().skip(1) {
            if start <= open_end {
                open_end = open_end.max(end);
            } else {
                seconds += (open_end - open_start) as f64 / 1000.0;
                open_start = start;
                open_end = end;
            }
        }
        seconds += (open_end - open_start) as f64 / 1000.0;
        voice_seconds.push((actor, seconds));
    }

    // Weekly-active humans: one message or 600 deduplicated voice seconds.
    let mut active: HashSet<&str> = HashSet::new();
    for f in &eligible {
        if f.row.event_type == "message_created" {
            if let Some(actor) = f.row.actor_id.as_deref() {
                active.insert(actor);
            }
        }
    }
    for (actor, seconds) in &voice_seconds {
        if *seconds >= VOICE_ACTIVE_SECONDS {
            active.insert(actor.as_str());
        }
    }

    let human_messages = eligible
        .iter()
        .filter(|f| f.row.event_type == "message_created")
        .count() as u64;
    let joins: Vec<&ParsedFact> = eligible
        .iter()
        .filter(|f| f.row.event_type == "member_joined")
        .copied()
        .collect();

    // Attendance: verified proofs only, deduped by occurrence:actor
    // (legacy `attendance`; `rsvp` proof never counts).
    let mut participations: HashSet<String> = HashSet::new();
    let mut attendance_humans: HashSet<&str> = HashSet::new();
    for f in &facts {
        if f.row.event_type != "event_attended"
            || f.classification != "eligible_human"
            || f.row.actor_id.as_deref().is_none_or(str::is_empty)
        {
            continue;
        }
        let proof = meta_str(&f.meta, "proof").unwrap_or_default();
        if !["host_checkin", "durable_checkin", "voice_600s"].contains(&proof.as_str()) {
            continue;
        }
        let occurrence =
            meta_str(&f.meta, "eventOccurrenceId").unwrap_or_else(|| f.row.source_event_id.clone());
        let actor = f.row.actor_id.as_deref().unwrap_or_default();
        participations.insert(format!("{occurrence}:{actor}"));
        attendance_humans.insert(actor);
    }

    // Bot noise over human spaces (legacy: automated bot/webhook/staff
    // messages with `channelClass == 'human'`).
    let automated_messages = facts
        .iter()
        .filter(|f| {
            f.row.event_type == "message_created"
                && ["bot", "webhook", "staff_automation"].contains(&f.classification)
                && f.meta.get("channelClass").and_then(|v| v.as_str()) == Some("human")
        })
        .count() as u64;
    let eligible_human_space = eligible
        .iter()
        .filter(|f| {
            f.row.event_type == "message_created"
                && f.meta.get("channelClass").and_then(|v| v.as_str()) == Some("human")
        })
        .count() as u64;
    let bot_denominator = automated_messages + eligible_human_space;
    let bot_ratio = if bot_denominator == 0 {
        None
    } else {
        Some(automated_messages as f64 / bot_denominator as f64)
    };
    let bot_alert = bot_ratio.is_some_and(|r| r >= BOT_NOISE_THRESHOLD);

    // First-human-reply clock (legacy `firstHumanReply`).
    let mut accepted: HashMap<&str, &str> = HashMap::new();
    for f in &facts {
        if f.row.event_type != "rules_accepted" || f.classification != "eligible_human" {
            continue;
        }
        let Some(actor) = f.row.actor_id.as_deref() else {
            continue;
        };
        accepted
            .entry(actor)
            .and_modify(|cur| {
                if f.row.occurred_at.as_str() < *cur {
                    *cur = f.row.occurred_at.as_str();
                }
            })
            .or_insert(f.row.occurred_at.as_str());
    }
    let mut channel_messages: Vec<&ParsedFact> = facts
        .iter()
        .filter(|f| {
            f.row.event_type == "message_created"
                && f.classification == "eligible_human"
                && f.row.actor_id.as_deref().is_some_and(|a| !a.is_empty())
                && ["welcome", "human"].contains(
                    &f.meta
                        .get("channelClass")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                )
        })
        .collect();
    channel_messages.sort_by(|a, b| a.row.occurred_at.cmp(&b.row.occurred_at));
    let mut durations: Vec<f64> = Vec::new();
    let mut no_reply_within_24h = 0usize;
    let mut pending = 0usize;
    for join in &joins {
        let actor = join.row.actor_id.as_deref().unwrap_or_default();
        let clock_start = accepted
            .get(actor)
            .copied()
            .unwrap_or(join.row.occurred_at.as_str());
        let first_own = channel_messages.iter().find(|m| {
            m.row.actor_id.as_deref() == Some(actor) && m.row.occurred_at.as_str() >= clock_start
        });
        let Some(first_own) = first_own else {
            let start_ms = parse_iso_millis(clock_start).unwrap_or(i64::MIN);
            if week_end_ms.saturating_sub(start_ms) > FIRST_REPLY_BREACH_MS {
                no_reply_within_24h += 1;
            } else {
                pending += 1;
            }
            continue;
        };
        match channel_messages.iter().find(|m| {
            m.row.actor_id.as_deref() != Some(actor)
                && m.row.occurred_at > first_own.row.occurred_at
        }) {
            Some(reply) => {
                let start_ms = parse_iso_millis(clock_start).unwrap_or(0);
                let reply_ms = parse_iso_millis(&reply.row.occurred_at).unwrap_or(start_ms);
                durations.push((reply_ms.saturating_sub(start_ms)) as f64 / 1000.0);
            }
            None => {
                let start_ms = parse_iso_millis(clock_start).unwrap_or(i64::MIN);
                if week_end_ms.saturating_sub(start_ms) > FIRST_REPLY_BREACH_MS {
                    no_reply_within_24h += 1;
                } else {
                    pending += 1;
                }
            }
        }
    }
    let reply = FirstHumanReply {
        median_seconds: median(&durations),
        resolved_count: durations.len(),
        eligible_join_count: joins.len(),
        no_reply_within_24h_count: no_reply_within_24h,
        pending_count: pending,
    };

    // Evidence: eligible message/join/attendance actors + 600s voice actors.
    let mut humans: HashSet<&str> = HashSet::new();
    for f in &facts {
        if f.classification != "eligible_human"
            || !["message_created", "member_joined", "event_attended"]
                .contains(&f.row.event_type.as_str())
        {
            continue;
        }
        if let Some(actor) = f.row.actor_id.as_deref() {
            humans.insert(actor);
        }
    }
    for (actor, seconds) in &voice_seconds {
        if *seconds >= VOICE_ACTIVE_SECONDS {
            humans.insert(actor.as_str());
        }
    }
    let evidence_state = if humans.len() < MIN_EVIDENCE_HUMANS {
        "insufficient"
    } else {
        "sufficient"
    };

    // Intervention selection (legacy `selectIntervention`).
    let selected = if coverage_state == "incomplete" {
        (
            "INGESTION_INCOMPLETE",
            "repair ingestion/reconciliation before changing community programming",
        )
    } else if bot_alert {
        (
            "BOT_NOISE_HIGH",
            "pause or reduce one discretionary automated post source in human spaces",
        )
    } else if evidence_state == "insufficient" {
        (
            "none_insufficient_evidence",
            "fewer than five eligible humans; no growth intervention",
        )
    } else if reply.no_reply_within_24h_count > 0 {
        (
            "FIRST_REPLY_BREACH",
            "tighten the human welcome rota for the next week",
        )
    } else {
        (
            "HOLD",
            "no threshold crossed; continue the current one intervention",
        )
    };
    let invalid_recommendation = !["INGESTION_INCOMPLETE", "none_insufficient_evidence"]
        .contains(&selected.0)
        && (coverage_state == "incomplete" || evidence_state == "insufficient");
    let kill_switch_active = inputs.correction_cycles >= 2
        && (coverage_state == "incomplete"
            || reconciliation.values().any(|b| !b.reconciles)
            || invalid_recommendation);
    let recommendations_enabled = inputs.recommendations_enabled && !kill_switch_active;
    let intervention = if recommendations_enabled {
        Intervention {
            code: selected.0.to_owned(),
            reason: selected.1.to_owned(),
        }
    } else if coverage_state == "incomplete" {
        Intervention {
            code: "INGESTION_INCOMPLETE".to_owned(),
            reason: "recommendations and threshold notifications disabled; repair ingestion/reconciliation".to_owned(),
        }
    } else if evidence_state == "insufficient" {
        Intervention {
            code: "none_insufficient_evidence".to_owned(),
            reason: "recommendations and threshold notifications disabled; fewer than five eligible humans".to_owned(),
        }
    } else {
        Intervention {
            code: "HOLD".to_owned(),
            reason: "recommendations and threshold notifications disabled by kill switch"
                .to_owned(),
        }
    };

    let complete = coverage_state == "complete";
    let scorecard = Scorecard {
        guild_id: inputs.guild_id.clone(),
        week_start: inputs.week_start.clone(),
        week_end: inputs.week_end.clone(),
        generated_at: inputs.generated_at.clone(),
        classifier_version: inputs.classifier_version.clone(),
        watermark: inputs.watermark,
        idempotency_key: idempotency_key.clone(),
        revision: inputs.existing_revision_count + 1,
        coverage_state: coverage_state.to_owned(),
        evidence_state: evidence_state.to_owned(),
        raw_fact_count: facts.len(),
        weekly_active_humans: complete.then_some(active.len()),
        human_messages: complete.then_some(human_messages),
        eligible_joins: complete.then_some(joins.len() as u64),
        join_sources: complete.then_some(JoinSources {
            known: joins
                .iter()
                .filter(|j| !["unknown", ""].contains(&j.row.source.as_str()))
                .count() as u64,
            unknown: joins
                .iter()
                .filter(|j| ["unknown", ""].contains(&j.row.source.as_str()))
                .count() as u64,
        }),
        event_attendance: complete.then_some(EventAttendance {
            participations: participations.len(),
            distinct_humans: attendance_humans.len(),
        }),
        bot_noise: complete.then_some(BotNoise {
            numerator: automated_messages,
            denominator: bot_denominator,
            ratio: bot_ratio,
            alert: bot_alert,
        }),
        first_human_reply: complete.then_some(reply),
        exclusion_counts,
        reconciliation,
        ingestion_errors,
        intervention,
        recommendations_enabled,
        kill_switch_active,
    };

    // Threshold alerts (legacy): HOLD and insufficient-evidence never notify;
    // bot-noise pins the 0.20 line into the dedupe key, others use `contract`.
    let alert_key = if recommendations_enabled
        && !["HOLD", "none_insufficient_evidence"].contains(&scorecard.intervention.code.as_str())
    {
        let threshold = if scorecard.intervention.code == "BOT_NOISE_HIGH" {
            "0.20"
        } else {
            "contract"
        };
        Some(format!(
            "{}:{week_key}:{threshold}",
            scorecard.intervention.code
        ))
    } else {
        None
    };

    let scorecard_json = serde_json::to_string(&scorecard).expect("scorecard serializes to JSON");
    let input_count = facts.len();
    Ok(ScorecardOutcome {
        scorecard_json,
        reused: false,
        alert_key,
        scorecard: Some(scorecard),
        input_hash: Some(input_hash),
        input_count: Some(input_count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: &str = "guild-a";
    const VERSION: &str = "community-test-v1";
    const WEEK_START: &str = "2026-08-31T00:00:00.000Z";
    const WEEK_END: &str = "2026-09-07T00:00:00.000Z";
    const GENERATED_AT: &str = "2026-09-07T06:15:00.000Z";

    fn ms(iso: &str) -> i64 {
        parse_iso_millis(iso).expect("fixture timestamp parses")
    }

    fn input(actor: &str, is_bot: bool) -> ClassifyInput {
        ClassifyInput {
            guild_id: GUILD.to_owned(),
            actor_id: actor.to_owned(),
            is_bot,
            webhook_id: None,
            is_staff_automation: false,
            is_raid: false,
            is_staging: false,
            is_test: false,
        }
    }

    #[test]
    fn closed_week_boundary_and_schedule_are_exact() {
        // Monday 2026-09-07 06:15 UTC closes [2026-08-31, 2026-09-07).
        assert_eq!(
            previous_closed_week(ms(GENERATED_AT)),
            (WEEK_START.to_owned(), WEEK_END.to_owned())
        );
        assert!(!is_scorecard_run_time(ms("2026-09-07T06:14:59.000Z")));
        assert!(is_scorecard_run_time(ms(GENERATED_AT)));
        assert!(is_scorecard_run_time(ms("2026-09-07T06:59:59.000Z")));
        assert!(!is_scorecard_run_time(ms("2026-09-07T07:00:00.000Z")));
        assert!(!is_scorecard_run_time(ms("2026-09-08T06:15:00.000Z")));
        assert!(!is_scorecard_run_time(ms("2026-09-06T06:15:00.000Z")));
    }

    #[test]
    fn captured_streams_match_live_writers() {
        // The scorecard claims exactly the streams the runtime captures: a
        // stream in `CAPTURED_STREAMS` with no writer would invent coverage,
        // and a writer missing from the list would leave its stream unmarked.
        let registry: Vec<&str> = STREAM_WRITERS.iter().map(|(s, _)| *s).collect();
        assert_eq!(registry.len(), COMMUNITY_FACT_TYPES.len());
        for stream in COMMUNITY_FACT_TYPES {
            assert!(registry.contains(&stream), "{stream} has no registry row");
        }
        let claimed: HashSet<&str> = CAPTURED_STREAMS.iter().copied().collect();
        assert_eq!(
            claimed.len(),
            CAPTURED_STREAMS.len(),
            "CAPTURED_STREAMS lists a stream twice"
        );
        for (stream, writer) in STREAM_WRITERS {
            assert_eq!(
                claimed.contains(stream),
                writer.is_some(),
                "stream {stream}: claimed without a writer, or written without a claim"
            );
        }
    }

    #[test]
    fn tick_fires_each_monday_exactly_once() {
        // Week boundary: Sunday night never fires, Monday 06:15 fires once.
        assert_eq!(scorecard_tick(ms("2026-09-06T23:59:00.000Z"), None), None);
        let monday = ms("2026-09-07T06:15:00.000Z");
        assert_eq!(scorecard_tick(monday, None), Some("2026-09-07".to_owned()));
        // Same Monday, later tick: already attempted, no second fire.
        assert_eq!(
            scorecard_tick(ms("2026-09-07T06:45:00.000Z"), Some("2026-09-07")),
            None
        );
        // Next Monday fires again under its own key.
        assert_eq!(
            scorecard_tick(ms("2026-09-14T06:15:00.000Z"), Some("2026-09-07")),
            Some("2026-09-14".to_owned())
        );
    }

    #[test]
    fn classifier_precedence_matches_legacy() {
        let cfg = ClassifierConfig::from_map(
            &[
                ("TWO_COMMUNITY_AUTOMATION_ACTOR_IDS", "staff-bot"),
                ("TWO_COMMUNITY_RAID_ACTOR_IDS", "raid-user"),
                ("TWO_COMMUNITY_STAGING_GUILD_IDS", "staging-guild"),
                ("TWO_COMMUNITY_STAGING_ACTOR_IDS", "staging-user"),
                ("TWO_COMMUNITY_TEST_ACTOR_IDS", "test-user"),
            ]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        );
        // A bot posting through a webhook is one bot bucket, not two.
        let both = ClassifyInput {
            webhook_id: Some("w".to_owned()),
            ..input("x", true)
        };
        assert_eq!(classify(&cfg, &both).classification, "bot");
        let hook = ClassifyInput {
            webhook_id: Some("w".to_owned()),
            ..input("x", false)
        };
        assert_eq!(classify(&cfg, &hook).classification, "webhook");
        assert_eq!(
            classify(&cfg, &input("staff-bot", false)).classification,
            "staff_automation"
        );
        assert_eq!(
            classify(&cfg, &input("raid-user", false)).classification,
            "raid"
        );
        let staged = ClassifyInput {
            guild_id: "staging-guild".to_owned(),
            ..input("x", false)
        };
        assert_eq!(classify(&cfg, &staged).classification, "staging");
        assert_eq!(
            classify(&cfg, &input("test-user", false)).classification,
            "test"
        );
        let human = classify(&cfg, &input("human-1", false));
        assert_eq!(human.classification, "eligible_human");
        assert_eq!(human.matched_rule, "no_exclusion_matched");
    }

    fn fact(
        id: i64,
        event_type: &str,
        actor: Option<&str>,
        occurred_at: &str,
        classification: &str,
        metadata: Option<&str>,
    ) -> FactRow {
        FactRow {
            id,
            guild_id: GUILD.to_owned(),
            event_type: event_type.to_owned(),
            source_event_id: format!("src-{id}"),
            actor_id: actor.map(str::to_owned),
            occurred_at: occurred_at.to_owned(),
            source: "channel:general".to_owned(),
            classifier_version: VERSION.to_owned(),
            classification: classification.to_owned(),
            matched_rule: "fixture".to_owned(),
            metadata: metadata.map(str::to_owned),
            idempotency_key: format!("key-{id}"),
        }
    }

    fn cover() -> Vec<StreamCoverage> {
        COMMUNITY_FACT_TYPES
            .iter()
            .map(|s| StreamCoverage {
                stream: (*s).to_owned(),
                covered_from: WEEK_START.to_owned(),
                covered_through: WEEK_END.to_owned(),
            })
            .collect()
    }

    fn inputs(facts: Vec<FactRow>) -> ScorecardInputs {
        ScorecardInputs {
            guild_id: GUILD.to_owned(),
            classifier_version: VERSION.to_owned(),
            week_start: WEEK_START.to_owned(),
            week_end: WEEK_END.to_owned(),
            watermark: facts.iter().map(|f| f.id).max().unwrap_or(0),
            generated_at: GENERATED_AT.to_owned(),
            recommendations_enabled: true,
            correction_cycles: 0,
            facts,
            coverage: cover(),
            duplicate_source_ids: false,
            existing_revision_count: 0,
            existing_scorecard_json: None,
        }
    }

    fn build(facts: Vec<FactRow>) -> Scorecard {
        build_scorecard(inputs(facts))
            .expect("builds")
            .scorecard
            .expect("fresh build")
    }

    #[test]
    fn incomplete_coverage_fails_closed_with_null_numerators() {
        // A process started mid-week cannot cover the whole closed week.
        let mut partial = cover();
        partial[0].covered_from = "2026-09-03T00:00:00.000Z".to_owned();
        let mut inp = inputs(vec![]);
        inp.coverage = partial;
        let out = build_scorecard(inp).expect("builds");
        let scorecard = out.scorecard.expect("fresh build");
        assert_eq!(scorecard.coverage_state, "incomplete");
        assert_eq!(scorecard.weekly_active_humans, None);
        assert_eq!(scorecard.human_messages, None);
        assert_eq!(scorecard.intervention.code, "INGESTION_INCOMPLETE");
        assert!(scorecard
            .ingestion_errors
            .iter()
            .any(|e| e.starts_with("missing_stream_coverage:")));
    }

    #[test]
    fn first_reply_uses_occurrence_order_for_backfilled_messages() {
        let own = r#"{"channelClass":"welcome"}"#;
        let reply = r#"{"channelClass":"human"}"#;
        for late_actor in ["helper", "newcomer"] {
            let scorecard = build(vec![
                fact(
                    1,
                    "member_joined",
                    Some("newcomer"),
                    "2026-09-01T00:00:00.000Z",
                    "eligible_human",
                    None,
                ),
                fact(
                    2,
                    "message_created",
                    Some(late_actor),
                    "2026-09-03T00:00:00.000Z",
                    "eligible_human",
                    Some(own),
                ),
                fact(
                    3,
                    "message_created",
                    Some("newcomer"),
                    "2026-09-01T00:30:00.000Z",
                    "eligible_human",
                    Some(own),
                ),
                fact(
                    4,
                    "message_created",
                    Some("helper"),
                    "2026-09-01T01:00:00.000Z",
                    "eligible_human",
                    Some(reply),
                ),
            ]);
            let reply = scorecard.first_human_reply.expect("complete coverage");
            assert_eq!(reply.median_seconds, Some(3600.0), "{late_actor}");
            assert_eq!(reply.resolved_count, 1);
            assert_eq!(reply.no_reply_within_24h_count, 0);
            assert_eq!(reply.pending_count, 0);
        }
    }

    #[test]
    fn missing_voice_duration_is_coverage_exempt_but_not_activity() {
        let missing = r#"{"startedAt":"2026-09-02T10:00:00.000Z"}"#;
        let null = r#"{"startedAt":"2026-09-02T10:00:00.000Z","durationSeconds":null}"#;
        for (metadata, expected_active) in [(missing, 0), (null, 5)] {
            let facts = ["a", "b", "c", "d", "e"]
                .iter()
                .enumerate()
                .map(|(i, actor)| {
                    fact(
                        i as i64 + 1,
                        "voice_session_ended",
                        Some(actor),
                        "2026-09-02T10:10:00.000Z",
                        "eligible_human",
                        Some(metadata),
                    )
                })
                .collect();
            let scorecard = build(facts);
            assert_eq!(scorecard.coverage_state, "complete");
            assert!(scorecard.ingestion_errors.is_empty());
            assert_eq!(scorecard.weekly_active_humans, Some(expected_active));
            assert_eq!(
                scorecard.evidence_state,
                if expected_active == 0 {
                    "insufficient"
                } else {
                    "sufficient"
                }
            );
        }
    }

    #[test]
    fn invalid_voice_duration_still_fails_coverage_closed() {
        for duration in [r#""invalid""#, "-1"] {
            let metadata = format!(
                r#"{{"startedAt":"2026-09-02T10:00:00.000Z","durationSeconds":{duration}}}"#
            );
            let scorecard = build(vec![fact(
                1,
                "voice_session_ended",
                Some("human"),
                "2026-09-02T10:10:00.000Z",
                "eligible_human",
                Some(&metadata),
            )]);
            assert_eq!(scorecard.coverage_state, "incomplete");
            assert_eq!(scorecard.weekly_active_humans, None);
            assert!(scorecard
                .ingestion_errors
                .iter()
                .any(|e| e == "invalid_voice_duration"));
        }
    }

    #[test]
    fn five_voice_only_actors_are_sufficient_evidence() {
        // Five humans with 600s+ deduplicated voice each, no messages at all.
        let mut facts = Vec::new();
        for (i, actor) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let id = i as i64 + 1;
            facts.push(FactRow {
                metadata: Some(
                    r#"{"sessionKey":"s","channelId":"v","startedAt":"2026-09-02T10:00:00.000Z","durationSeconds":600,"startKnown":true}"#.to_owned(),
                ),
                occurred_at: "2026-09-02T10:10:00.000Z".to_owned(),
                ..fact(id, "voice_session_ended", Some(actor), "", "eligible_human", None)
            });
        }
        let scorecard = build(facts);
        assert_eq!(scorecard.evidence_state, "sufficient");
        assert_eq!(scorecard.weekly_active_humans, Some(5));
    }

    #[test]
    fn bot_noise_alert_fires_at_twenty_percent() {
        // 4 eligible + 1 automated human-space message = exactly 20%.
        let mut facts: Vec<FactRow> = (0..4)
            .map(|i| {
                fact(
                    i + 1,
                    "message_created",
                    Some(&format!("human-{i}")),
                    "2026-09-02T10:00:00.000Z",
                    "eligible_human",
                    Some(r#"{"channelId":"general","channelClass":"human"}"#),
                )
            })
            .collect();
        // Two more eligible humans so evidence is sufficient and the only
        // signal left is the noise ratio.
        for i in 4..6 {
            facts.push(fact(
                i + 1,
                "member_joined",
                Some(&format!("human-{i}")),
                "2026-09-02T10:00:00.000Z",
                "eligible_human",
                None,
            ));
        }
        facts.push(fact(
            7,
            "message_created",
            Some("staff-bot"),
            "2026-09-02T11:00:00.000Z",
            "staff_automation",
            Some(r#"{"channelId":"general","channelClass":"human"}"#),
        ));
        let scorecard = build(facts);
        let noise = scorecard
            .bot_noise
            .expect("complete coverage reports noise");
        assert_eq!((noise.numerator, noise.denominator), (1, 5));
        assert!((noise.ratio.unwrap_or_default() - 0.2).abs() < f64::EPSILON);
        assert!(noise.alert);
        assert_eq!(scorecard.intervention.code, "BOT_NOISE_HIGH");
    }

    #[test]
    fn duplicate_rerun_reuses_result_without_alert() {
        let facts = vec![fact(
            1,
            "message_created",
            Some("human-1"),
            "2026-09-02T10:00:00.000Z",
            "eligible_human",
            Some(r#"{"channelId":"general","channelClass":"human"}"#),
        )];
        let mut inp = inputs(facts);
        inp.existing_scorecard_json = Some(r#"{"reused":true}"#.to_owned());
        let out = build_scorecard(inp).expect("builds");
        assert!(out.reused);
        assert_eq!(out.alert_key, None);
        assert_eq!(out.scorecard_json, r#"{"reused":true}"#);
        assert_eq!(out.scorecard, None);
    }

    #[test]
    fn kill_switch_disables_recommendations_after_two_corrections() {
        // Incomplete ingestion plus two operator corrections: HOLD-equivalent
        // INGESTION_INCOMPLETE with recommendations off.
        let mut inp = inputs(vec![]);
        inp.coverage = vec![];
        inp.correction_cycles = 2;
        let out = build_scorecard(inp).expect("builds");
        let scorecard = out.scorecard.expect("fresh build");
        assert!(scorecard.kill_switch_active);
        assert!(!scorecard.recommendations_enabled);
        assert_eq!(scorecard.intervention.code, "INGESTION_INCOMPLETE");
        assert_eq!(out.alert_key, None);
    }

    #[test]
    fn gates_default_off_with_recommendations_on() {
        let gates = ScorecardGates::from_map(&HashMap::new()).expect("defaults");
        assert!(!gates.enabled);
        assert!(gates.recommendations_enabled);
        assert_eq!(gates.correction_cycles, 0);
        let vars: HashMap<String, String> = [
            ("TWO_COMMUNITY_SCORECARD", "1"),
            ("TWO_COMMUNITY_RECOMMENDATIONS", "0"),
            ("TWO_COMMUNITY_CORRECTION_CYCLES", "2"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let gates = ScorecardGates::from_map(&vars).expect("parses");
        assert!(gates.enabled && !gates.recommendations_enabled);
        assert_eq!(gates.correction_cycles, 2);
    }
}
