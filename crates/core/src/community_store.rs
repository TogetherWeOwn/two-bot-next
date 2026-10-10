//! Community scorecard + fact sqlx store (TOG-10092).
//!
//! Ports the `community_facts` writes this slice owns (`recordMessage`,
//! `recordMemberJoin`, `recordRulesAccepted`, `recordVoiceStarted/Ended` —
//! the attendance write already lives in the RSVP slice, TOG-10083), the
//! stream heartbeats (`markStreamCoverage`), and the Monday run persistence
//! (`buildCommunityScorecard` insert + threshold-alert dedupe) onto the 0311
//! tables (`community_facts` itself lands in 0160, applied alongside).
//! The pure build stays in [`crate::community`]: this module only moves rows,
//! keyed by the helpers there, so the SQL is a thin transliteration of the
//! legacy queries.
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The gateway handlers (S3) call the fact writes through the `FactsSink`
//! seam, the 60s Monday scheduler calls [`run_closed_week`]; until the S4
//! router/executor slices land, the outcome types are the integration surface
//! — no private dispatcher or HTTP client lives here.
//!
//! Legacy table/column names are kept exactly. `occurred_at`/`covered_*` are
//! TEXT holding ISO-8601 UTC, so coverage comparisons are lexicographic
//! (correct for that format).

use sqlx::{Pool, Postgres};

use super::community::{
    build_scorecard, Classification, ClassifyInput, FactRow, ScorecardInputs, ScorecardOutcome,
    StreamCoverage, COMMUNITY_FACT_TYPES,
};

/// Community store failure: transport plus the one domain parse a stored row
/// can still trigger (a fact-type outside the legacy CHECK, e.g. written by hand).
#[derive(Debug, thiserror::Error)]
pub enum CommunityStoreError {
    #[error("community store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("stored community fact type {0:?} is not a known stream")]
    UnknownFactType(String),
    #[error("scorecard build failed: {0}")]
    Build(String),
}

/// One fact write. The classifier verdict travels with the row so the store
/// stays a thin transliteration; S3 supplies it via [`crate::community::classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactWrite {
    pub guild_id: String,
    pub event_type: String,
    pub source_event_id: String,
    pub actor_id: Option<String>,
    pub occurred_at: String,
    pub source: String,
    pub classification: Classification,
    pub metadata: Option<String>,
    pub idempotency_key: String,
}

/// Insert one fact. Returns `true` when inserted, `false` when the idempotency
/// key already held it (legacy `ON CONFLICT DO NOTHING RETURNING id`).
pub async fn record_fact(
    pool: &Pool<Postgres>,
    fact: &FactWrite,
) -> Result<bool, CommunityStoreError> {
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO community_facts
           (guild_id, event_type, source_event_id, actor_id, occurred_at, source,
            classifier_version, classification, matched_rule, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(&fact.guild_id)
    .bind(&fact.event_type)
    .bind(&fact.source_event_id)
    .bind(&fact.actor_id)
    .bind(&fact.occurred_at)
    .bind(&fact.source)
    .bind(&fact.classification.classifier_version)
    .bind(&fact.classification.classification)
    .bind(&fact.classification.matched_rule)
    .bind(&fact.metadata)
    .bind(&fact.idempotency_key)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// Message fact (legacy `recordMessage` inputs, minus the classifier).
#[must_use]
pub fn message_fact(
    guild_id: &str,
    message_id: &str,
    channel_id: &str,
    channel_class: &str,
    actor: &ClassifyInput,
    occurred_at: &str,
    classification: Classification,
) -> FactWrite {
    FactWrite {
        guild_id: guild_id.to_owned(),
        event_type: "message_created".to_owned(),
        source_event_id: message_id.to_owned(),
        actor_id: Some(actor.actor_id.clone()),
        occurred_at: occurred_at.to_owned(),
        source: format!("channel:{channel_id}"),
        classification,
        metadata: Some(format!(
            "{{\"channelId\":\"{channel_id}\",\"channelClass\":\"{channel_class}\",\"webhookId\":{},\"discordBot\":{}}}",
            actor.webhook_id.as_deref().map_or("null".to_owned(), |w| format!("\"{w}\"")),
            actor.is_bot
        )),
        idempotency_key: format!("discord-message:{message_id}"),
    }
}

/// Member-join fact (legacy `recordMemberJoin`).
#[must_use]
pub fn member_join_fact(
    guild_id: &str,
    actor: &ClassifyInput,
    occurred_at: &str,
    source_event_id: &str,
    source: &str,
    classification: Classification,
    metadata: Option<String>,
) -> FactWrite {
    FactWrite {
        guild_id: guild_id.to_owned(),
        event_type: "member_joined".to_owned(),
        source_event_id: source_event_id.to_owned(),
        actor_id: Some(actor.actor_id.clone()),
        occurred_at: occurred_at.to_owned(),
        source: source.to_owned(),
        classification,
        metadata,
        idempotency_key: format!("member-join:{guild_id}:{}:{occurred_at}", actor.actor_id),
    }
}

/// Rules-accepted fact (legacy `recordRulesAccepted`).
#[must_use]
pub fn rules_accepted_fact(
    guild_id: &str,
    actor: &ClassifyInput,
    occurred_at: &str,
    source_event_id: &str,
    source: &str,
    classification: Classification,
    metadata: Option<String>,
) -> FactWrite {
    FactWrite {
        guild_id: guild_id.to_owned(),
        event_type: "rules_accepted".to_owned(),
        source_event_id: source_event_id.to_owned(),
        actor_id: Some(actor.actor_id.clone()),
        occurred_at: occurred_at.to_owned(),
        source: source.to_owned(),
        classification,
        metadata,
        idempotency_key: format!("rules-accepted:{guild_id}:{}", actor.actor_id),
    }
}

/// Voice-start fact, returning the durable session key (legacy
/// `recordVoiceStarted`).
#[must_use]
pub fn voice_started_fact(
    guild_id: &str,
    actor: &ClassifyInput,
    channel_id: &str,
    occurred_at: &str,
    session_key: Option<String>,
    classification: Classification,
) -> (String, FactWrite) {
    let key = session_key
        .unwrap_or_else(|| format!("{guild_id}:{}:{occurred_at}:{channel_id}", actor.actor_id));
    let write = FactWrite {
        guild_id: guild_id.to_owned(),
        event_type: "voice_session_started".to_owned(),
        source_event_id: key.clone(),
        actor_id: Some(actor.actor_id.clone()),
        occurred_at: occurred_at.to_owned(),
        source: format!("channel:{channel_id}"),
        classification,
        metadata: Some(format!(
            "{{\"sessionKey\":\"{key}\",\"channelId\":\"{channel_id}\"}}"
        )),
        idempotency_key: format!("voice-start:{key}"),
    };
    (key, write)
}

/// Voice-end fact (legacy `recordVoiceEnded`).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn voice_ended_fact(
    guild_id: &str,
    actor: &ClassifyInput,
    session_key: &str,
    channel_id: &str,
    occurred_at: &str,
    started_at: Option<&str>,
    duration_seconds: Option<i64>,
    classification: Classification,
) -> FactWrite {
    let metadata = format!(
        "{{\"sessionKey\":\"{session_key}\",\"channelId\":\"{channel_id}\",\"startedAt\":{},\"durationSeconds\":{},\"startKnown\":{}}}",
        started_at.map_or("null".to_owned(), |s| format!("\"{s}\"")),
        duration_seconds.map_or("null".to_owned(), |d| d.to_string()),
        started_at.is_some() && duration_seconds.is_some()
    );
    FactWrite {
        guild_id: guild_id.to_owned(),
        event_type: "voice_session_ended".to_owned(),
        source_event_id: session_key.to_owned(),
        actor_id: Some(actor.actor_id.clone()),
        occurred_at: occurred_at.to_owned(),
        source: format!("channel:{channel_id}"),
        classification,
        metadata: Some(metadata),
        idempotency_key: format!("voice-end:{session_key}"),
    }
}

/// Record one gateway voice-start fact: build via [`voice_started_fact`] and
/// insert via [`record_fact`]. Returns the durable session key plus whether
/// the `voice-start:{session_key}` row was newly inserted (`false` when the
/// same join redelivers and the key already holds it).
pub async fn record_voice_started_fact(
    pool: &Pool<Postgres>,
    guild_id: &str,
    actor: &super::community::ClassifyInput,
    channel_id: &str,
    occurred_at: &str,
    session_key: Option<String>,
    classification: super::community::Classification,
) -> Result<(String, bool), CommunityStoreError> {
    let (key, write) = voice_started_fact(
        guild_id,
        actor,
        channel_id,
        occurred_at,
        session_key,
        classification,
    );
    let inserted = record_fact(pool, &write).await?;
    Ok((key, inserted))
}

/// Record one gateway voice-end fact: build via [`voice_ended_fact`] and
/// insert via [`record_fact`]. Returns `true` when inserted, `false` when the
/// `voice-end:{session_key}` key already held it (duplicate leave delivery).
/// An end without a seen start carries `startedAt: null`,
/// `durationSeconds: null`, `startKnown: false` — never a fabricated start.
#[allow(clippy::too_many_arguments)]
pub async fn record_voice_ended_fact(
    pool: &Pool<Postgres>,
    guild_id: &str,
    actor: &super::community::ClassifyInput,
    session_key: &str,
    channel_id: &str,
    occurred_at: &str,
    started_at: Option<&str>,
    duration_seconds: Option<i64>,
    classification: super::community::Classification,
) -> Result<bool, CommunityStoreError> {
    let write = voice_ended_fact(
        guild_id,
        actor,
        session_key,
        channel_id,
        occurred_at,
        started_at,
        duration_seconds,
        classification,
    );
    record_fact(pool, &write).await
}

/// Upsert one stream heartbeat (legacy `markStreamCoverage`).
pub async fn mark_stream_coverage(
    pool: &Pool<Postgres>,
    guild_id: &str,
    stream: &str,
    covered_from: &str,
    covered_through: &str,
    updated_at: &str,
) -> Result<(), CommunityStoreError> {
    sqlx::query(
        "INSERT INTO community_stream_heartbeats
           (guild_id, stream, covered_from, covered_through, updated_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (guild_id, stream) DO UPDATE
           SET covered_from = excluded.covered_from,
               covered_through = excluded.covered_through,
               updated_at = excluded.updated_at",
    )
    .bind(guild_id)
    .bind(stream)
    .bind(covered_from)
    .bind(covered_through)
    .bind(updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Closed-week load parameters (legacy `buildCommunityScorecard` reads).
#[derive(Debug, Clone)]
pub struct ScorecardLoad {
    pub guild_id: String,
    pub classifier_version: String,
    pub week_start: String,
    pub week_end: String,
    pub watermark: i64,
    pub generated_at: String,
    pub recommendations_enabled: bool,
    pub correction_cycles: u32,
}

/// One scoped fact row, in column order.
type FactColumns = (
    i64,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);

/// Load the builder inputs for one closed week: scoped facts ordered by `id`,
/// heartbeats, the duplicate probe, the revision count, and any existing row
/// behind the idempotency key.
pub async fn load_scorecard_inputs(
    pool: &Pool<Postgres>,
    load: &ScorecardLoad,
) -> Result<ScorecardInputs, CommunityStoreError> {
    let rows: Vec<FactColumns> = sqlx::query_as(
        "SELECT id, guild_id, event_type, source_event_id, actor_id, occurred_at, source,
                classifier_version, classification, matched_rule, metadata, idempotency_key
           FROM community_facts
          WHERE guild_id = $1 AND occurred_at >= $2 AND occurred_at < $3 AND id <= $4
          ORDER BY id",
    )
    .bind(&load.guild_id)
    .bind(&load.week_start)
    .bind(&load.week_end)
    .bind(load.watermark)
    .fetch_all(pool)
    .await?;
    let mut facts = Vec::with_capacity(rows.len());
    for (
        id,
        guild_id,
        event_type,
        source_event_id,
        actor_id,
        occurred_at,
        source,
        classifier_version,
        classification,
        matched_rule,
        metadata,
        idempotency_key,
    ) in rows
    {
        if !COMMUNITY_FACT_TYPES.contains(&event_type.as_str()) {
            return Err(CommunityStoreError::UnknownFactType(event_type));
        }
        facts.push(FactRow {
            id,
            guild_id,
            event_type,
            source_event_id,
            actor_id,
            occurred_at,
            source,
            classifier_version,
            classification,
            matched_rule,
            metadata,
            idempotency_key,
        });
    }
    let coverage_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT stream, covered_from, covered_through FROM community_stream_heartbeats
          WHERE guild_id = $1",
    )
    .bind(&load.guild_id)
    .fetch_all(pool)
    .await?;
    let coverage = coverage_rows
        .into_iter()
        .map(|(stream, covered_from, covered_through)| StreamCoverage {
            stream,
            covered_from,
            covered_through,
        })
        .collect();
    let duplicates: Vec<(String,)> = sqlx::query_as(
        "SELECT source_event_id FROM community_facts
          WHERE guild_id = $1 AND id <= $2
          GROUP BY source_event_id, event_type
         HAVING COUNT(*) > 1 LIMIT 1",
    )
    .bind(&load.guild_id)
    .bind(load.watermark)
    .fetch_all(pool)
    .await?;
    let idempotency_key = format!(
        "community-health:{}:{}:{}:{}",
        load.guild_id,
        &load.week_start[..10],
        load.classifier_version,
        load.watermark
    );
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT scorecard_json FROM community_scorecard_runs WHERE idempotency_key = $1",
    )
    .bind(&idempotency_key)
    .fetch_optional(pool)
    .await?;
    let revision_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM community_scorecard_runs
          WHERE guild_id = $1 AND week_start = $2 AND classifier_version = $3",
    )
    .bind(&load.guild_id)
    .bind(&load.week_start)
    .bind(&load.classifier_version)
    .fetch_one(pool)
    .await?;
    Ok(ScorecardInputs {
        guild_id: load.guild_id.clone(),
        classifier_version: load.classifier_version.clone(),
        week_start: load.week_start.clone(),
        week_end: load.week_end.clone(),
        watermark: load.watermark,
        generated_at: load.generated_at.clone(),
        recommendations_enabled: load.recommendations_enabled,
        correction_cycles: load.correction_cycles,
        facts,
        coverage,
        duplicate_source_ids: !duplicates.is_empty(),
        existing_revision_count: revision_count.0.max(0) as u64,
        existing_scorecard_json: existing.map(|(json,)| json),
    })
}

/// Persist one built scorecard (legacy `buildCommunityScorecard` tail):
/// insert the run row (idempotent on the key), then attempt the threshold
/// alert dedupe. Returns `(reused, alert_emitted)`.
pub async fn persist_scorecard_run(
    pool: &Pool<Postgres>,
    outcome: &ScorecardOutcome,
) -> Result<(bool, bool), CommunityStoreError> {
    if outcome.reused {
        return Ok((true, false));
    }
    let scorecard = outcome.scorecard.as_ref().ok_or_else(|| {
        CommunityStoreError::Build("fresh outcome carries no scorecard".to_owned())
    })?;
    let mut tx = pool.begin().await?;
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO community_scorecard_runs
           (guild_id, week_start, week_end, classifier_version, watermark, input_count, input_hash,
            idempotency_key, revision, run_status, coverage_state, evidence_state, scorecard_json,
            intervention_code, generated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(&scorecard.guild_id)
    .bind(&scorecard.week_start)
    .bind(&scorecard.week_end)
    .bind(&scorecard.classifier_version)
    .bind(scorecard.watermark)
    .bind(outcome.input_count.unwrap_or(0) as i64)
    .bind(outcome.input_hash.as_deref().unwrap_or(""))
    .bind(&scorecard.idempotency_key)
    .bind(scorecard.revision as i64)
    .bind(if scorecard.coverage_state == "complete" {
        "completed"
    } else {
        "incomplete"
    })
    .bind(&scorecard.coverage_state)
    .bind(&scorecard.evidence_state)
    .bind(&outcome.scorecard_json)
    .bind(&scorecard.intervention.code)
    .bind(&scorecard.generated_at)
    .fetch_optional(&mut *tx)
    .await?;
    let inserted = inserted.is_some();
    let mut alert_emitted = false;
    if inserted {
        if let Some(alert_key) = outcome.alert_key.as_deref() {
            let won: Option<(String,)> = sqlx::query_as(
                "INSERT INTO community_scorecard_alerts (guild_id, week_start, alert_key, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (guild_id, alert_key) DO NOTHING RETURNING alert_key",
            )
            .bind(&scorecard.guild_id)
            .bind(&scorecard.week_start)
            .bind(alert_key)
            .bind(&scorecard.generated_at)
            .fetch_optional(&mut *tx)
            .await?;
            alert_emitted = won.is_some();
        }
    }
    tx.commit().await?;
    Ok((!inserted, alert_emitted))
}

/// One Monday run for the previous closed week (legacy
/// `runPreviousClosedCommunityWeek`): watermark at the guild's max fact id,
/// build, persist. Returns `(scorecard_json, reused, alert_emitted)`.
pub async fn run_closed_week(
    pool: &Pool<Postgres>,
    load: ScorecardLoad,
) -> Result<(String, bool, bool), CommunityStoreError> {
    let watermark: (Option<i64>,) =
        sqlx::query_as("SELECT MAX(id) FROM community_facts WHERE guild_id = $1")
            .bind(&load.guild_id)
            .fetch_one(pool)
            .await?;
    let load = ScorecardLoad {
        watermark: watermark.0.unwrap_or(0),
        ..load
    };
    let inputs = load_scorecard_inputs(pool, &load).await?;
    let outcome = build_scorecard(inputs).map_err(|e| CommunityStoreError::Build(e.to_string()))?;
    let json = outcome.scorecard_json.clone();
    let (reused, alert_emitted) = persist_scorecard_run(pool, &outcome).await?;
    Ok((json, reused, alert_emitted))
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
        };
        test_pool_with_url(prefix, &url).await.map(Some)
    }

    async fn test_pool_with_url(
        prefix: &str,
        url: &str,
    ) -> Result<(Pool<Postgres>, String), sqlx::Error> {
        let options: PgConnectOptions = url.parse()?;
        // No DATABASE_URL or inherited credentials: tests accept only the
        // approved agent container or the ephemeral Postgres service in
        // GitHub Actions (see the `community-db` CI job).
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        // Use the runtime migration chain, including 0160's community_facts,
        // rather than a test-only table that could hide migration conflicts.
        sqlx::migrate!("../cutover/migrations").run(&pool).await?;
        Ok((pool, schema))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    const GUILD: &str = "guild-a";
    const VERSION: &str = "community-test-v1";
    const WEEK_START: &str = "2026-08-31T00:00:00.000Z";
    const WEEK_END: &str = "2026-09-07T00:00:00.000Z";
    const GENERATED_AT: &str = "2026-09-07T06:15:00.000Z";

    fn actor(id: &str) -> ClassifyInput {
        ClassifyInput {
            guild_id: GUILD.to_owned(),
            actor_id: id.to_owned(),
            is_bot: false,
            webhook_id: None,
            is_staff_automation: false,
            is_raid: false,
            is_staging: false,
            is_test: false,
        }
    }

    fn verdict(classification: &str) -> Classification {
        Classification {
            classification: classification.to_owned(),
            classifier_version: VERSION.to_owned(),
            matched_rule: "fixture".to_owned(),
        }
    }

    async fn seed_fixture(pool: &Pool<Postgres>) {
        // The mixed legacy fixture: 7 messages (1 human + 6 excluded), 2
        // joins, 3 human voice ends (incl. an overlapping 600s union pair).
        // Actor/time/classification shapes match the golden run exactly
        // (human message at 10:00, excluded messages at 11:00).
        for (id, who, class) in [
            ("m-human", "human", "eligible_human"),
            ("m-bot", "m-bot", "bot"),
            ("m-webhook", "m-webhook", "webhook"),
            ("m-staff", "m-staff", "staff_automation"),
            ("m-raid", "m-raid", "raid"),
            ("m-staging", "m-staging", "staging"),
            ("m-test", "m-test", "test"),
        ] {
            let at = if id == "m-human" {
                "2026-09-01T10:00:00.000Z"
            } else {
                "2026-09-01T11:00:00.000Z"
            };
            let input = actor(who);
            let write = message_fact(GUILD, id, "general", "human", &input, at, verdict(class));
            record_fact(pool, &write).await.expect("seed message");
        }
        for (id, who, at, class) in [
            (
                "j-human",
                "join-human",
                "2026-09-02T10:00:00.000Z",
                "eligible_human",
            ),
            ("j-raid", "join-raid", "2026-09-02T11:00:00.000Z", "raid"),
        ] {
            let write =
                member_join_fact(GUILD, &actor(who), at, id, "unknown", verdict(class), None);
            record_fact(pool, &write).await.expect("seed join");
        }
        for (id, who, start, end) in [
            (
                "v-human",
                "voice-human",
                "2026-09-03T10:00:00.000Z",
                "2026-09-03T10:10:00.000Z",
            ),
            (
                "v-600-a",
                "voice-600",
                "2026-09-02T10:00:00.000Z",
                "2026-09-02T10:06:00.000Z",
            ),
            (
                "v-600-b",
                "voice-600",
                "2026-09-02T10:05:00.000Z",
                "2026-09-02T10:10:00.000Z",
            ),
        ] {
            let duration = (super::super::funnel::parse_iso_millis(end).expect("end parses")
                - super::super::funnel::parse_iso_millis(start).expect("start parses"))
                / 1000;
            let write = voice_ended_fact(
                GUILD,
                &actor(who),
                id,
                "voice",
                end,
                Some(start),
                Some(duration),
                verdict("eligible_human"),
            );
            record_fact(pool, &write).await.expect("seed voice");
        }
        // Full-week coverage on every stream, like the legacy job persists
        // before scoring the closed week.
        for stream in COMMUNITY_FACT_TYPES {
            mark_stream_coverage(pool, GUILD, stream, WEEK_START, WEEK_END, GENERATED_AT)
                .await
                .expect("seed coverage");
        }
    }

    #[tokio::test]
    async fn monday_run_matches_legacy_golden_output() {
        let Some((pool, schema)) = test_pool("tog_10092_scorecard")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping community_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        seed_fixture(&pool).await;
        let load = ScorecardLoad {
            guild_id: GUILD.to_owned(),
            classifier_version: VERSION.to_owned(),
            week_start: WEEK_START.to_owned(),
            week_end: WEEK_END.to_owned(),
            watermark: 0,
            generated_at: GENERATED_AT.to_owned(),
            recommendations_enabled: true,
            correction_cycles: 0,
        };
        let (json, reused, alerted) = run_closed_week(&pool, load.clone())
            .await
            .expect("monday run builds");
        assert!(!reused);
        // Golden values from the real legacy build (legacy-scorecard.json,
        // generated from frozen two-bot @ d5d11793 against agent-testdb):
        // 12 facts, complete coverage, insufficient evidence (3 humans),
        // BOT_NOISE_HIGH at 3/4 automated human-space messages.
        let scorecard: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(scorecard["coverageState"], "complete");
        assert_eq!(scorecard["evidenceState"], "insufficient");
        assert_eq!(scorecard["rawFactCount"], 12);
        assert_eq!(scorecard["weeklyActiveHumans"], 3);
        assert_eq!(scorecard["humanMessages"], 1);
        assert_eq!(scorecard["eligibleJoins"], 1);
        assert_eq!(
            scorecard["joinSources"],
            serde_json::json!({"known": 0, "unknown": 1})
        );
        assert_eq!(
            scorecard["eventAttendance"],
            serde_json::json!({"participations": 0, "distinctHumans": 0})
        );
        assert_eq!(
            scorecard["botNoise"],
            serde_json::json!({"numerator": 3, "denominator": 4, "ratio": 0.75, "alert": true})
        );
        assert_eq!(
            scorecard["firstHumanReply"],
            serde_json::json!({"medianSeconds": null, "resolvedCount": 0, "eligibleJoinCount": 1, "noReplyWithin24hCount": 1, "pendingCount": 0})
        );
        assert_eq!(scorecard["intervention"]["code"], "BOT_NOISE_HIGH");
        assert_eq!(scorecard["ingestionErrors"], serde_json::json!([]));
        assert_eq!(scorecard["reconciliation"]["message_created"]["raw"], 7);
        assert_eq!(scorecard["reconciliation"]["total"]["raw"], 12);
        assert_eq!(scorecard["reconciliation"]["total"]["eligible_human"], 5);
        assert!(scorecard["reconciliation"]["total"]["reconciles"]
            .as_bool()
            .unwrap_or(false));
        assert!(alerted, "threshold alert emits on the first run");
        // Duplicate rerun reuses the persisted row and emits no second alert.
        let (json2, reused2, alerted2) =
            run_closed_week(&pool, load).await.expect("duplicate rerun");
        assert!(reused2);
        assert!(!alerted2);
        assert_eq!(json2, json);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn midweek_start_fails_closed_with_null_numerators() {
        let Some((pool, schema)) = test_pool("tog_10092_failclosed")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping community_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // A process started mid-week cannot cover the whole closed week.
        for stream in COMMUNITY_FACT_TYPES {
            mark_stream_coverage(
                &pool,
                GUILD,
                stream,
                "2026-09-03T00:00:00.000Z",
                WEEK_END,
                GENERATED_AT,
            )
            .await
            .expect("partial coverage");
        }
        let load = ScorecardLoad {
            guild_id: GUILD.to_owned(),
            classifier_version: VERSION.to_owned(),
            week_start: WEEK_START.to_owned(),
            week_end: WEEK_END.to_owned(),
            watermark: 0,
            generated_at: GENERATED_AT.to_owned(),
            recommendations_enabled: true,
            correction_cycles: 0,
        };
        let (json, reused, alerted) = run_closed_week(&pool, load)
            .await
            .expect("fail-closed run builds");
        // Fresh run, never a reuse; the incomplete-ingestion alert still
        // emits (legacy `selected.code !== 'HOLD'` gate), so operators hear
        // about the gap instead of silence.
        assert!(!reused);
        assert!(alerted);
        let scorecard: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(scorecard["coverageState"], "incomplete");
        assert_eq!(scorecard["weeklyActiveHumans"], serde_json::Value::Null);
        assert_eq!(scorecard["humanMessages"], serde_json::Value::Null);
        assert_eq!(scorecard["intervention"]["code"], "INGESTION_INCOMPLETE");
        drop_schema(&pool, &schema).await;
    }

    #[test]
    fn voice_session_key_and_end_metadata_shapes_are_honest() {
        // Session-key shape: the start keys on guild, member, stamp and
        // channel, so a redelivered join dedupes on the same idempotency key.
        let input = actor("human-1");
        let at = "2026-09-02T10:00:00.000Z";
        let (key, start) = voice_started_fact(
            GUILD,
            &input,
            "voice-9",
            at,
            None,
            verdict("eligible_human"),
        );
        assert_eq!(key, format!("{GUILD}:human-1:{at}:voice-9"));
        assert_eq!(start.event_type, "voice_session_started");
        assert_eq!(start.source_event_id, key);
        assert_eq!(start.idempotency_key, format!("voice-start:{key}"));
        assert_eq!(start.source, "channel:voice-9");
        let start_meta = start.metadata.expect("start metadata");
        assert!(
            start_meta.contains(&format!("\"sessionKey\":\"{key}\"")),
            "start carries its session key: {start_meta}"
        );

        // A supplied key survives (the gateway tracker's durable key), so a
        // move's end and the next start never share a row.
        let (supplied, _) = voice_started_fact(
            GUILD,
            &input,
            "voice-9",
            at,
            Some("durable-key".to_owned()),
            verdict("eligible_human"),
        );
        assert_eq!(supplied, "durable-key");

        // Measured end: all four metadata fields travel together.
        let end = voice_ended_fact(
            GUILD,
            &input,
            &key,
            "voice-9",
            "2026-09-02T10:05:30.000Z",
            Some(at),
            Some(330),
            verdict("eligible_human"),
        );
        assert_eq!(end.event_type, "voice_session_ended");
        assert_eq!(end.source_event_id, key);
        assert_eq!(end.idempotency_key, format!("voice-end:{key}"));
        let end_meta = end.metadata.expect("end metadata");
        for field in [
            format!("\"sessionKey\":\"{key}\"").as_str(),
            "\"startedAt\":\"2026-09-02T10:00:00.000Z\"",
            "\"durationSeconds\":330",
            "\"startKnown\":true",
        ] {
            assert!(end_meta.contains(field), "end carries {field}: {end_meta}");
        }

        // End without a seen start: honest unknown, never a fabricated start.
        let orphan = voice_ended_fact(
            GUILD,
            &input,
            "guild:member:unknown-start:2026-09-02T10:05:30.000Z:voice-9",
            "voice-9",
            "2026-09-02T10:05:30.000Z",
            None,
            None,
            verdict("eligible_human"),
        );
        let orphan_meta = orphan.metadata.expect("orphan end metadata");
        assert!(
            orphan_meta.contains("\"startedAt\":null")
                && orphan_meta.contains("\"durationSeconds\":null")
                && orphan_meta.contains("\"startKnown\":false"),
            "orphan end is honestly unknown: {orphan_meta}"
        );
        assert!(
            !orphan_meta.contains("2026-09-02T10:00:00.000Z"),
            "no start is invented: {orphan_meta}"
        );
    }

    /// Voice session start→end round-trip (TOG-19605): one session inserts a
    /// start and an honestly-measured end, a duplicate start dedupes on
    /// `voice-start:{session_key}`, and both stream heartbeats are present.
    /// Routed to a `check.yml` ignored-db-runtime step.
    #[tokio::test]
    #[ignore = "needs a disposable test database; routed to a check.yml step"]
    async fn voice_session_start_end_round_trip_is_idempotent() {
        let Some((pool, schema)) = test_pool("tog_19605_voice")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping community_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let at = "2026-09-02T10:00:00.000Z";
        let end_at = "2026-09-02T10:05:30.000Z";
        let input = actor("human-1");
        let (key, first) = record_voice_started_fact(
            &pool,
            GUILD,
            &input,
            "voice",
            at,
            None,
            verdict("eligible_human"),
        )
        .await
        .expect("first start inserts");
        assert!(first, "first join inserts");
        assert_eq!(key, format!("{GUILD}:human-1:{at}:voice"));
        // Duplicate join redelivery: the same session key dedupes to false.
        let (dup_key, retry) = record_voice_started_fact(
            &pool,
            GUILD,
            &input,
            "voice",
            at,
            None,
            verdict("eligible_human"),
        )
        .await
        .expect("duplicate start");
        assert_eq!(dup_key, key, "duplicate delivery reuses the session key");
        assert!(!retry, "duplicate join returns false");
        // Honest duration math: 330 whole seconds for the 5.5-minute session.
        let inserted = record_voice_ended_fact(
            &pool,
            GUILD,
            &input,
            &key,
            "voice",
            end_at,
            Some(at),
            Some(330),
            verdict("eligible_human"),
        )
        .await
        .expect("measured end inserts");
        assert!(inserted, "measured end inserts");
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT event_type, source_event_id, metadata FROM community_facts
             WHERE guild_id = $1 ORDER BY id",
        )
        .bind(GUILD)
        .fetch_all(&pool)
        .await
        .expect("voice rows");
        assert_eq!(rows.len(), 2, "one start plus one end, no double row");
        assert_eq!(rows[0].0, "voice_session_started");
        assert_eq!(rows[0].1, key);
        assert_eq!(rows[1].0, "voice_session_ended");
        assert_eq!(rows[1].1, key);
        assert!(
            rows[1].2.contains(&format!("\"sessionKey\":\"{key}\""))
                && rows[1]
                    .2
                    .contains("\"startedAt\":\"2026-09-02T10:00:00.000Z\"")
                && rows[1].2.contains("\"durationSeconds\":330")
                && rows[1].2.contains("\"startKnown\":true"),
            "end row carries honest measurement: {}",
            rows[1].2
        );
        for stream in ["voice_session_started", "voice_session_ended"] {
            mark_stream_coverage(&pool, GUILD, stream, WEEK_START, WEEK_END, GENERATED_AT)
                .await
                .expect("voice heartbeat");
        }
        let beats: Vec<(String,)> = sqlx::query_as(
            "SELECT stream FROM community_stream_heartbeats WHERE guild_id = $1 AND stream LIKE 'voice_%' ORDER BY stream",
        )
        .bind(GUILD)
        .fetch_all(&pool)
        .await
        .expect("voice heartbeats present");
        assert_eq!(beats.len(), 2, "both voice stream heartbeats present");
        drop_schema(&pool, &schema).await;
    }
}
