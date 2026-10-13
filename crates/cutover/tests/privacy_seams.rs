//! Seam-level privacy cover for the write paths outside `record_event`.
//!
//! Review finding 4: only the cutover event writer and the service were
//! exercised with suppression or toggles. These tests seed a suppression row
//! and disabled families, then drive the gateway batch commit, the community
//! writer, the capture counter baseline, `record_earliest`,
//! `touch_activity` / `mark_bot`, and a writer racing an opt-out, asserting
//! no per-user rows survive for the suppressed pair while other members'
//! rows persist. The acceptance contract itself lives in
//! `privacy_opt_out.rs`, which this file never duplicates: every test here
//! targets a seam that file does not touch.

use two_bot_core::community::{Classification, ClassifyInput};
use two_bot_core::community_store::{
    member_join_fact, member_join_metadata, message_fact, record_fact, voice_ended_fact,
};
use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::{FunnelBatch, InviteSnapshotWrite};
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::rsvp::{checkin_classification, AttendanceProof};
use two_bot_core::rsvp_store::{record_checkin, CheckinWrite};
use two_bot_core::{EventType, FunnelEvent, InviteState};
use two_bot_cutover::db::{
    mark_bot, record_earliest, record_event, touch_activity, CutoverDb, FunnelWrite,
};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_cutover::invite_store::{store_counters, CounterRow};
use two_bot_cutover::privacy::{opt_out_member, set_analytics_settings};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const MEMBER_A: &str = "123456789012345678";
const MEMBER_B: &str = "123456789012345679";
const MEMBER_C: &str = "123456789012345680";

fn guild_u64() -> u64 {
    GUILD.parse().unwrap()
}

fn member_u64(member: &str) -> u64 {
    member.parse().unwrap()
}

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the privacy seams database suite"
            );
            eprintln!("SKIP privacy seams integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

async fn suppress(db: &CutoverDb, guild: &str, member: &str) {
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(guild)
        .bind(member)
        .execute(db.pool())
        .await
        .expect("suppression insert");
}

fn funnel_event(guild: &str, member: &str, event_type: &str, at: &str) -> FunnelWrite {
    FunnelWrite {
        member_id: Some(member.to_owned()),
        guild_id: guild.to_owned(),
        event_type: event_type.to_owned(),
        occurred_at: at.to_owned(),
        source: "privacy-seams-fixture".to_owned(),
        metadata: Some("{}".to_owned()),
    }
}

fn actor(member: &str) -> ClassifyInput {
    ClassifyInput {
        guild_id: GUILD.to_owned(),
        actor_id: member.to_owned(),
        is_bot: false,
        webhook_id: None,
        is_staff_automation: false,
        is_raid: false,
        is_staging: false,
        is_test: false,
    }
}

fn verdict() -> Classification {
    Classification {
        classification: "eligible_human".to_owned(),
        classifier_version: "privacy-seams-v1".to_owned(),
        matched_rule: "fixture".to_owned(),
    }
}

/// Community facts honor suppression for every fact type and toggles for
/// message/voice facts; join facts are suppression-only like other
/// lifecycle rows.
#[tokio::test]
async fn community_facts_gate_suppression_and_family_toggles() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let pool = db.pool();

    // Defaults: everything persists.
    let message = message_fact(
        GUILD,
        "msg-1",
        "chan-1",
        "human",
        &actor(MEMBER_B),
        "2026-10-13T00:00:00.000Z",
        verdict(),
    );
    assert!(
        record_fact(pool, &message).await.unwrap(),
        "default settings persist message facts"
    );

    // Activity off: message facts drop, voice facts persist.
    set_analytics_settings(&db, GUILD, true, false, true)
        .await
        .unwrap();
    let message2 = message_fact(
        GUILD,
        "msg-2",
        "chan-1",
        "human",
        &actor(MEMBER_B),
        "2026-10-13T00:01:00.000Z",
        verdict(),
    );
    assert!(
        !record_fact(pool, &message2).await.unwrap(),
        "disabled activity drops message facts"
    );
    let voice = voice_ended_fact(
        GUILD,
        &actor(MEMBER_B),
        "seam-session-1",
        "voice-1",
        "2026-10-13T00:10:00.000Z",
        Some("2026-10-13T00:00:00.000Z"),
        Some(600),
        verdict(),
    );
    assert!(
        record_fact(pool, &voice).await.unwrap(),
        "voice facts survive a disabled activity switch"
    );

    // Suppression drops every fact type for the pair only.
    suppress(&db, GUILD, MEMBER_A).await;
    for (label, fact) in [
        (
            "message",
            message_fact(
                GUILD,
                "msg-3",
                "chan-1",
                "human",
                &actor(MEMBER_A),
                "2026-10-13T00:02:00.000Z",
                verdict(),
            ),
        ),
        (
            "voice",
            voice_ended_fact(
                GUILD,
                &actor(MEMBER_A),
                "seam-session-2",
                "voice-1",
                "2026-10-13T00:11:00.000Z",
                Some("2026-10-13T00:01:00.000Z"),
                Some(600),
                verdict(),
            ),
        ),
        (
            "join",
            member_join_fact(
                GUILD,
                &actor(MEMBER_A),
                "2026-10-13T00:03:00.000Z",
                "1:2:2026-10-13T00:03:00.000Z",
                "invite:seam",
                verdict(),
                None,
            ),
        ),
    ] {
        assert!(
            !record_fact(pool, &fact).await.unwrap(),
            "suppressed pair drops {label} facts"
        );
    }
    let other = message_fact(
        GUILD,
        "msg-4",
        "chan-1",
        "human",
        &actor(MEMBER_B),
        "2026-10-13T00:04:00.000Z",
        verdict(),
    );
    // Activity is still disabled, so B's message drops on the toggle, not on
    // suppression; the voice fact below proves B still persists.
    assert!(!record_fact(pool, &other).await.unwrap());
    let other_voice = voice_ended_fact(
        GUILD,
        &actor(MEMBER_B),
        "seam-session-3",
        "voice-1",
        "2026-10-13T00:12:00.000Z",
        Some("2026-10-13T00:02:00.000Z"),
        Some(600),
        verdict(),
    );
    assert!(
        record_fact(pool, &other_voice).await.unwrap(),
        "other members keep persisting"
    );

    db.close().await;
    test_db.close().await.unwrap();
}

/// The opt-out purges community facts with the reviewed erasure predicate.
#[tokio::test]
async fn opt_out_purges_community_facts_for_the_pair_only() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let pool = db.pool();

    for (who, stamp) in [(MEMBER_A, "00:00:00"), (MEMBER_B, "00:01:00")] {
        let message = message_fact(
            GUILD,
            &format!("purge-{who}"),
            "chan-1",
            "human",
            &actor(who),
            &format!("2026-10-13T{stamp}.000Z"),
            verdict(),
        );
        assert!(record_fact(pool, &message).await.unwrap());
    }
    let join = member_join_fact(
        GUILD,
        &actor(MEMBER_A),
        "2026-10-13T00:02:00.000Z",
        "9:9:2026-10-13T00:02:00.000Z",
        "invite:seam",
        verdict(),
        member_join_metadata(Some(member_u64(MEMBER_B))),
    );
    assert!(record_fact(pool, &join).await.unwrap());

    let counts = opt_out_member(&db, GUILD, MEMBER_A, "operator:privacy-seams")
        .await
        .expect("opt-out commits");
    let community = counts
        .iter()
        .find(|c| c.table == "community_facts")
        .expect("community_facts is a purge table");
    assert_eq!(community.rows, 2, "message plus join facts for the pair");

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM community_facts WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(remaining, 1, "only the other member's fact survives");

    db.close().await;
    test_db.close().await.unwrap();
}

/// A suppressed inviter's attribution is blanked everywhere a join can land
/// it: the event writer, the community writer, and the capture baseline.
/// The joiner's own row still persists with the attribution stripped.
#[tokio::test]
async fn suppressed_inviter_attribution_is_blanked_not_repersisted() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    suppress(&db, GUILD, MEMBER_A).await;

    // Event writer: the join persists, the inviter does not.
    let mut join = funnel_event(GUILD, MEMBER_C, "member_join", "2026-10-13T00:00:00.000Z");
    join.metadata = Some(format!("{{\"inviterId\":\"{MEMBER_A}\"}}"));
    let (inserted, _) = record_event(&db, &join).await.unwrap();
    assert!(inserted, "the joiner's own row still lands");
    let stored: (Option<String>,) =
        sqlx::query_as("SELECT metadata FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_C)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        stored.0.as_deref().is_none_or(|m| !m.contains(MEMBER_A)),
        "suppressed inviter stripped from join metadata: {:?}",
        stored.0
    );

    // Community writer: same strip, same surviving fact.
    let fact = member_join_fact(
        GUILD,
        &actor(MEMBER_C),
        "2026-10-13T01:00:00.000Z",
        "7:8:2026-10-13T01:00:00.000Z",
        "invite:seam",
        verdict(),
        member_join_metadata(Some(member_u64(MEMBER_A))),
    );
    assert!(record_fact(db.pool(), &fact).await.unwrap());
    let fact_meta: (Option<String>,) = sqlx::query_as(
        "SELECT metadata FROM community_facts WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_C)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        fact_meta.0.as_deref().is_none_or(|m| !m.contains(MEMBER_A)),
        "suppressed inviter stripped from fact metadata: {:?}",
        fact_meta.0
    );

    // Capture baseline: uses keep counting, inviter blanks.
    store_counters(
        db.pool(),
        GUILD,
        &[CounterRow {
            code: "seam".to_owned(),
            uses: 5,
            inviter_id: Some(MEMBER_A.to_owned()),
            channel_id: None,
        }],
        "2026-10-13T00:00:00Z",
    )
    .await
    .unwrap();
    let baseline: (i32, Option<String>) = sqlx::query_as(
        "SELECT uses, inviter_id FROM invite_snapshots WHERE guild_id = $1 AND code = $2",
    )
    .bind(GUILD)
    .bind("seam")
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(baseline.0, 5, "aggregate uses keep counting");
    assert_eq!(baseline.1, None, "suppressed inviter blanked");

    db.close().await;
    test_db.close().await.unwrap();
}

/// The gateway batch commit drops suppressed per-user writes, honors disabled
/// families, and blanks suppressed inviters while other members persist.
#[tokio::test]
async fn gateway_batch_commit_gates_suppression_toggles_and_inviters() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    suppress(&db, GUILD, MEMBER_A).await;
    set_analytics_settings(&db, GUILD, true, true, false)
        .await
        .unwrap();

    let store = GatewaySessionStore::new(db.pool().clone(), GUILD.to_owned(), 0);
    let session = GatewaySession {
        session_id: "seam-session".into(),
        sequence: 1,
        resume_url: "ws://seam".into(),
        updated_at_ms: 1_760_000_000_000,
    };
    let at = "2026-10-13T00:00:00.000Z".to_owned();
    let batch_event = |member: Option<u64>, kind: EventType, stamp: &str| FunnelEvent {
        guild_id: guild_u64(),
        member_id: member,
        event_type: kind,
        occurred_at: stamp.to_owned(),
        source: "gateway".to_owned(),
        metadata: None,
        dedupe_token: None,
    };
    let mut join_by_c = batch_event(Some(member_u64(MEMBER_C)), EventType::MemberJoin, &at);
    join_by_c.metadata = Some(serde_json::json!({ "inviterId": MEMBER_A }));
    let batch = FunnelBatch {
        events: vec![
            batch_event(Some(member_u64(MEMBER_A)), EventType::FirstMessage, &at),
            batch_event(Some(member_u64(MEMBER_B)), EventType::FirstMessage, &at),
            batch_event(
                Some(member_u64(MEMBER_B)),
                EventType::VoiceSessionStart,
                "2026-10-13T00:01:00.000Z",
            ),
            join_by_c,
        ],
        activity: vec![
            (guild_u64(), member_u64(MEMBER_A), at.clone()),
            (guild_u64(), member_u64(MEMBER_B), at.clone()),
        ],
        bots: vec![
            (guild_u64(), member_u64(MEMBER_A)),
            (guild_u64(), member_u64(MEMBER_B)),
        ],
        snapshots: vec![],
        invite_snapshots: vec![InviteSnapshotWrite {
            guild_id: guild_u64(),
            states: vec![InviteState {
                code: "seam".to_owned(),
                uses: 5,
                inviter_id: Some(member_u64(MEMBER_A)),
                channel_id: None,
            }],
            observed_at: at.clone(),
            replace_all: false,
        }],
    };
    assert_eq!(
        store.commit_dispatch(&session, batch).await.unwrap(),
        DispatchAction::Apply
    );

    // Suppressed member: no event rows, no members row.
    let suppressed_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(suppressed_events, 0);
    let suppressed_member: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(suppressed_member, 0);

    // Other member: message persists, voice drops on the disabled family.
    let kept: Vec<(String,)> =
        sqlx::query_as("SELECT event_type FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_B)
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].0, "first_message");

    // Joiner's row persists with the suppressed inviter stripped.
    let join_meta: (Option<String>,) = sqlx::query_as(
        "SELECT metadata FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = 'member_join'",
    )
    .bind(GUILD)
    .bind(MEMBER_C)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        join_meta.0.as_deref().is_none_or(|m| !m.contains(MEMBER_A)),
        "batch strips suppressed inviters: {:?}",
        join_meta.0
    );

    // Snapshot: uses counted, inviter blanked.
    let snapshot: (i32, Option<String>) = sqlx::query_as(
        "SELECT uses, inviter_id FROM invite_snapshots WHERE guild_id = $1 AND code = $2",
    )
    .bind(GUILD)
    .bind("seam")
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(snapshot.0, 5);
    assert_eq!(snapshot.1, None);

    db.close().await;
    test_db.close().await.unwrap();
}

/// `record_earliest`, `touch_activity` and `mark_bot` drop suppressed pairs
/// while other members persist.
#[tokio::test]
async fn earliest_touch_and_mark_drop_suppressed_pairs() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    suppress(&db, GUILD, MEMBER_A).await;

    let join = funnel_event(GUILD, MEMBER_A, "member_join", "2026-10-13T00:00:00.000Z");
    assert_eq!(
        record_earliest(&db, &join).await.unwrap(),
        (false, None),
        "suppressed reconfirmation advances nothing"
    );
    touch_activity(&db, GUILD, MEMBER_A, "2026-10-13T00:00:00.000Z")
        .await
        .unwrap();
    mark_bot(&db, GUILD, MEMBER_A).await.unwrap();
    let suppressed_member: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(suppressed_member, 0, "bumps create no row for the pair");

    touch_activity(&db, GUILD, MEMBER_B, "2026-10-13T00:00:00.000Z")
        .await
        .unwrap();
    mark_bot(&db, GUILD, MEMBER_B).await.unwrap();
    let other_member: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_B)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(other_member, 1, "other members still bump");

    db.close().await;
    test_db.close().await.unwrap();
}

/// A writer in flight while the opt-out runs never lands after the purge:
/// the shared table lock serializes the two, so every write is either
/// dropped (opt-out first) or purged (writer first).
#[tokio::test]
async fn concurrent_write_during_opt_out_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    let writer_db = db.clone();
    let writer = tokio::spawn(async move {
        // `member_join` is repeatable per timestamp, so each iteration is a
        // distinct row racing the purge.
        for i in 0..50 {
            let at = format!("2026-10-13T00:{i:02}.000Z");
            let event = funnel_event(GUILD, MEMBER_A, "member_join", &at);
            record_event(&writer_db, &event).await.unwrap();
        }
    });
    let racer_db = db.clone();
    let opt_out = tokio::spawn(async move {
        opt_out_member(&racer_db, GUILD, MEMBER_A, "operator:privacy-race")
            .await
            .expect("racing opt-out commits")
    });
    let (writer_out, opt_out_out) = tokio::join!(writer, opt_out);
    writer_out.unwrap();
    assert_eq!(
        opt_out_out.unwrap().len(),
        4,
        "racing opt-out reports all purge tables"
    );

    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(events, 0, "no in-flight write survives the opt-out");
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(members, 0, "no projection row survives either");
    let suppression: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(suppression, 1, "the suppression itself installed");

    db.close().await;
    test_db.close().await.unwrap();
}

/// Host check-in drops suppressed pairs while other members still check in:
/// the next check-in after an opt-out must not undo the purge.
#[tokio::test]
async fn rsvp_checkin_drops_suppressed_pairs() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    suppress(&db, GUILD, MEMBER_A).await;

    let human = checkin_classification(false);
    let checkin = |member: &str, occurrence: &str| CheckinWrite {
        guild_id: GUILD.to_owned(),
        event_occurrence_id: occurrence.to_owned(),
        member_id: member.to_owned(),
        occurred_at: "2026-10-13T00:00:00.000Z".to_owned(),
        proof: AttendanceProof::HostCheckin,
        classifier_version: "privacy-seams-v1".to_owned(),
        classification: human.classification.to_owned(),
        matched_rule: human.matched_rule.to_owned(),
    };
    assert!(
        !record_checkin(db.pool(), &checkin(MEMBER_A, "occ-suppressed"))
            .await
            .unwrap(),
        "suppressed check-in drops"
    );
    assert!(
        record_checkin(db.pool(), &checkin(MEMBER_B, "occ-suppressed"))
            .await
            .unwrap(),
        "other members still check in"
    );

    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_facts WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(facts, 0, "no attendance fact for the suppressed pair");

    db.close().await;
    test_db.close().await.unwrap();
}

/// A community-fact writer in flight while the opt-out runs never lands
/// after the purge: removing the `record_fact` table lock lets rows survive,
/// so this race pins it.
#[tokio::test]
async fn concurrent_fact_write_during_opt_out_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        for i in 0..30 {
            let fact = message_fact(
                GUILD,
                &format!("race-{i}"),
                "chan-1",
                "human",
                &ClassifyInput {
                    guild_id: GUILD.to_owned(),
                    actor_id: MEMBER_A.to_owned(),
                    is_bot: false,
                    webhook_id: None,
                    is_staff_automation: false,
                    is_raid: false,
                    is_staging: false,
                    is_test: false,
                },
                &format!("2026-10-13T00:{i:02}:00.000Z"),
                Classification {
                    classification: "eligible_human".to_owned(),
                    classifier_version: "privacy-seams-v1".to_owned(),
                    matched_rule: "fixture".to_owned(),
                },
            );
            // Suppression drops return `Ok(false)`; a purge racing the
            // insert surfaces as a serialization/deadlock error on rare
            // timing — either way the pair must not survive below.
            let _ = record_fact(&pool, &fact).await;
        }
    });
    let racer_db = db.clone();
    let opt_out = tokio::spawn(async move {
        opt_out_member(&racer_db, GUILD, MEMBER_A, "operator:privacy-race-fact")
            .await
            .expect("racing opt-out commits")
    });
    let (writer_out, opt_out_out) = tokio::join!(writer, opt_out);
    writer_out.unwrap();
    assert_eq!(
        opt_out_out.unwrap().len(),
        4,
        "racing opt-out reports all purge tables"
    );

    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_facts WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(facts, 0, "no in-flight fact survives the opt-out");

    db.close().await;
    test_db.close().await.unwrap();
}

/// A gateway batch in flight while the opt-out runs never lands after the
/// purge: removing the batch commit's table lock lets event rows survive, so
/// this race pins it.
#[tokio::test]
async fn concurrent_gateway_batch_during_opt_out_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        let store = GatewaySessionStore::new(pool, GUILD.to_owned(), 0);
        for i in 0..20_u64 {
            let session = GatewaySession {
                session_id: format!("race-batch-{i}"),
                sequence: 1,
                resume_url: "ws://race".into(),
                updated_at_ms: 1_760_000_000_000_i64 + i as i64,
            };
            let batch = FunnelBatch {
                events: vec![FunnelEvent {
                    guild_id: guild_u64(),
                    member_id: Some(member_u64(MEMBER_A)),
                    event_type: EventType::MemberJoin,
                    occurred_at: format!("2026-10-13T00:{i:02}:00.000Z"),
                    source: "gateway-race".to_owned(),
                    metadata: None,
                    dedupe_token: None,
                }],
                activity: vec![],
                bots: vec![],
                snapshots: vec![],
                invite_snapshots: vec![],
            };
            let _ = store.commit_dispatch(&session, batch).await;
        }
    });
    let racer_db = db.clone();
    let opt_out = tokio::spawn(async move {
        opt_out_member(&racer_db, GUILD, MEMBER_A, "operator:privacy-race-batch")
            .await
            .expect("racing opt-out commits")
    });
    let (writer_out, opt_out_out) = tokio::join!(writer, opt_out);
    writer_out.unwrap();
    assert_eq!(
        opt_out_out.unwrap().len(),
        4,
        "racing opt-out reports all purge tables"
    );

    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(events, 0, "no in-flight batch event survives the opt-out");

    db.close().await;
    test_db.close().await.unwrap();
}

/// A counter baseline in flight while the opt-out runs never re-attributes
/// the member: removing the `store_counters` table lock (or its suppression
/// filter) leaves a row naming the suppressed inviter, so this race pins
/// both.
#[tokio::test]
async fn concurrent_counter_write_during_opt_out_never_attributing() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        for i in 0..30 {
            let _ = store_counters(
                &pool,
                GUILD,
                &[CounterRow {
                    code: "race".to_owned(),
                    uses: i,
                    inviter_id: Some(MEMBER_A.to_owned()),
                    channel_id: None,
                }],
                "2026-10-13T00:00:00Z",
            )
            .await;
        }
    });
    let racer_db = db.clone();
    let opt_out = tokio::spawn(async move {
        opt_out_member(&racer_db, GUILD, MEMBER_A, "operator:privacy-race-counters")
            .await
            .expect("racing opt-out commits")
    });
    let (writer_out, opt_out_out) = tokio::join!(writer, opt_out);
    writer_out.unwrap();
    assert_eq!(
        opt_out_out.unwrap().len(),
        4,
        "racing opt-out reports all purge tables"
    );

    let attributed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invite_snapshots WHERE guild_id = $1 AND inviter_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        attributed, 0,
        "no counter row names the suppressed inviter after the race"
    );

    db.close().await;
    test_db.close().await.unwrap();
}
