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

use two_bot_core::analytics_family::AnalyticsCategory;
use two_bot_core::community::{Classification, ClassifyInput};
use two_bot_core::community_store::{
    member_join_fact, member_join_metadata, message_fact, record_fact, voice_ended_fact,
    MessageFactInput,
};
use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::{ActivityTouch, FunnelBatch, InviteSnapshotWrite};
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::inactivity_store::run_sweep;
use two_bot_core::onboarding::{build_session_picks, plan_game_selection, plan_session};
use two_bot_core::onboarding_store::{
    begin_prompt, record_channel_routed, record_game_selected,
    record_game_selection_in_transaction, record_session_routed,
    record_session_routed_in_transaction,
};
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
    let message = message_fact(MessageFactInput {
        guild_id: GUILD,
        message_id: "msg-1",
        channel_id: "chan-1",
        channel_class: "human",
        actor: &actor(MEMBER_B),
        occurred_at: "2026-10-13T00:00:00.000Z",
        content_length: None,
        classification: verdict(),
    });
    assert!(
        record_fact(pool, &message).await.unwrap(),
        "default settings persist message facts"
    );

    // Activity off: message facts drop, voice facts persist.
    set_analytics_settings(&db, GUILD, true, false, true)
        .await
        .unwrap();
    let message2 = message_fact(MessageFactInput {
        guild_id: GUILD,
        message_id: "msg-2",
        channel_id: "chan-1",
        channel_class: "human",
        actor: &actor(MEMBER_B),
        occurred_at: "2026-10-13T00:01:00.000Z",
        content_length: None,
        classification: verdict(),
    });
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
            message_fact(MessageFactInput {
                guild_id: GUILD,
                message_id: "msg-3",
                channel_id: "chan-1",
                channel_class: "human",
                actor: &actor(MEMBER_A),
                occurred_at: "2026-10-13T00:02:00.000Z",
                content_length: None,
                classification: verdict(),
            }),
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
    let other = message_fact(MessageFactInput {
        guild_id: GUILD,
        message_id: "msg-4",
        channel_id: "chan-1",
        channel_class: "human",
        actor: &actor(MEMBER_B),
        occurred_at: "2026-10-13T00:04:00.000Z",
        content_length: None,
        classification: verdict(),
    });
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
        let message = message_fact(MessageFactInput {
            guild_id: GUILD,
            message_id: &format!("purge-{who}"),
            channel_id: "chan-1",
            channel_class: "human",
            actor: &actor(who),
            occurred_at: &format!("2026-10-13T{stamp}.000Z"),
            content_length: None,
            classification: verdict(),
        });
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
        // Message-origin touches: the suppressed member drops on
        // suppression, the allowed member persists on the enabled
        // activity family (voice is disabled in this fixture).
        activity: vec![
            ActivityTouch {
                guild_id: guild_u64(),
                member_id: member_u64(MEMBER_A),
                at: at.clone(),
                family: AnalyticsCategory::Activity,
            },
            ActivityTouch {
                guild_id: guild_u64(),
                member_id: member_u64(MEMBER_B),
                at: at.clone(),
                family: AnalyticsCategory::Activity,
            },
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
                max_uses: 0,
                temporary: false,
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
            let fact = message_fact(MessageFactInput {
                guild_id: GUILD,
                message_id: &format!("race-{i}"),
                channel_id: "chan-1",
                channel_class: "human",
                actor: &ClassifyInput {
                    guild_id: GUILD.to_owned(),
                    actor_id: MEMBER_A.to_owned(),
                    is_bot: false,
                    webhook_id: None,
                    is_staff_automation: false,
                    is_raid: false,
                    is_staging: false,
                    is_test: false,
                },
                occurred_at: &format!("2026-10-13T00:{i:02}:00.000Z"),
                content_length: None,
                classification: Classification {
                    classification: "eligible_human".to_owned(),
                    classifier_version: "privacy-seams-v1".to_owned(),
                    matched_rule: "fixture".to_owned(),
                },
            });
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
/// both. Each iteration uses a distinct code so a late write cannot be
/// overwritten (or deleted as absent) by a later iteration that sees the
/// suppression; only the suppression filter plus the lock keep every row
/// clean.
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
                    code: format!("race-{i}"),
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

/// Deterministic race helper: hold the opt-out's lock set open with the
/// suppression installed but uncommitted, start one writer (its gate reads
/// before the commit, then it blocks on the table lock), commit, and assert
/// nothing lands. Without the writer's `LOCK TABLE` the gate passes and the
/// later `INSERT` waits on the opt-out's `SHARE ROW EXCLUSIVE` lock, then
/// lands after the purge — so a single iteration pins the lock with no
/// timing dependence. Each test below covers one lock site added for the
/// opt-out; deleting its `LOCK TABLE` turns the final assertion red.
const OPT_OUT_LOCK_SQL: &str = "LOCK TABLE community_facts, events, invite_snapshots, members, privacy_opt_outs, privacy_opt_out_audit IN SHARE ROW EXCLUSIVE MODE";

async fn hold_opt_out_open<'a>(
    pool: &'a sqlx::PgPool,
    guild: &'a str,
    member: &'a str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut opt_tx = pool.begin().await.expect("opt-out tx");
    sqlx::query(OPT_OUT_LOCK_SQL)
        .execute(&mut *opt_tx)
        .await
        .expect("opt-out lock");
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(guild)
        .bind(member)
        .execute(&mut *opt_tx)
        .await
        .expect("suppression install");
    opt_tx
}

/// `touch_activity` gated under the shared lock: a bump that passes the gate
/// before the opt-out commits must never create the `members` row after it.
#[tokio::test]
async fn touch_activity_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let writer_db = db.clone();
    let writer = tokio::spawn(async move {
        touch_activity(&writer_db, GUILD, MEMBER_A, "2026-10-13T00:00:00.000Z").await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    writer.await.expect("writer task").expect("writer ok");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, 0, "touch_activity racing the opt-out creates no row");
    db.close().await;
    test_db.close().await.unwrap();
}

/// `mark_bot` gated under the shared lock: same pin as `touch_activity`.
#[tokio::test]
async fn mark_bot_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let writer_db = db.clone();
    let writer = tokio::spawn(async move { mark_bot(&writer_db, GUILD, MEMBER_A).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    writer.await.expect("writer task").expect("writer ok");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, 0, "mark_bot racing the opt-out creates no row");
    db.close().await;
    test_db.close().await.unwrap();
}

/// RSVP host check-in gated under the shared lock: the attendance fact for a
/// suppressed pair must never land after the purge.
#[tokio::test]
async fn rsvp_checkin_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        let human = checkin_classification(false);
        record_checkin(
            &pool,
            &CheckinWrite {
                guild_id: GUILD.to_owned(),
                event_occurrence_id: "occ-race".to_owned(),
                member_id: MEMBER_A.to_owned(),
                occurred_at: "2026-10-13T00:00:00.000Z".to_owned(),
                proof: AttendanceProof::HostCheckin,
                classifier_version: "privacy-seams-v1".to_owned(),
                classification: human.classification.to_owned(),
                matched_rule: human.matched_rule.to_owned(),
            },
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    let _ = writer.await.expect("writer task").expect("writer ok");
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_facts WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(facts, 0, "check-in racing the opt-out writes no fact");
    db.close().await;
    test_db.close().await.unwrap();
}

/// Onboarding writers gated under the shared lock: `begin_prompt` (plus the
/// guarded `record_sent`), the game/channel/session writers and both
/// in-transaction variants. Each writer runs once while the opt-out holds its
/// locks open; without the writer's lock its gate passes and the `INSERT`
/// lands after the purge. The two in-transaction writers race in separate
/// rounds: a combined round would let the first writer absorb the wait and
/// leave the second writer's lock unpinned.
#[tokio::test]
async fn onboarding_writers_deterministic_race_never_persist() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let pool = db.pool().clone();

    // begin_prompt + record_sent: the full welcome sequence races the opt-out.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            match begin_prompt(&writer_pool, GUILD, MEMBER_A).await {
                Ok(Some(guard)) => {
                    guard
                        .record_sent("chan-1", "2026-10-13T00:00:00.000Z")
                        .await
                }
                Ok(None) => Ok(false),
                Err(e) => Err(e),
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        let _ = writer.await.expect("writer task").expect("writer ok");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "prompt racing the opt-out writes no row");
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    // record_game_selected.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            record_game_selected(
                &writer_pool,
                GUILD,
                MEMBER_A,
                &["shooters".to_owned()],
                "2026-10-13T00:00:00.000Z",
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        let _ = writer.await.expect("writer task").expect("writer ok");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "game selection racing the opt-out writes no row");
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    // record_channel_routed.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let selected = plan_game_selection(&["shooters"], &|_| true);
            record_channel_routed(
                &writer_pool,
                GUILD,
                MEMBER_A,
                &selected,
                "2026-10-13T00:00:00.000Z",
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        let _ = writer.await.expect("writer task").expect("writer ok");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "channel routing racing the opt-out writes no row");
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    // record_session_routed.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let picks = build_session_picks("10", "11");
            let session = plan_session(&["find-players"], &|_| true, &picks);
            record_session_routed(
                &writer_pool,
                GUILD,
                MEMBER_A,
                &session,
                "2026-10-13T00:00:00.000Z",
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        let _ = writer.await.expect("writer task").expect("writer ok");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "session routing racing the opt-out writes no row");
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    // In-transaction game selection races the opt-out alone in the caller's
    // role-write transaction, so its own lock absorbs the wait.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let mut caller_tx = writer_pool.begin().await.expect("caller tx");
            let selected = plan_game_selection(&["shooters"], &|_| true);
            record_game_selection_in_transaction(
                &mut caller_tx,
                GUILD,
                MEMBER_A,
                &["shooters".to_owned()],
                &selected,
                "2026-10-13T00:00:00.000Z",
            )
            .await
            .expect("in-tx game write");
            caller_tx.commit().await.expect("caller commit");
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows, 0,
            "in-transaction game selection racing the opt-out writes no row"
        );
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    // In-transaction session routing races the opt-out alone: a combined
    // round would let the game write absorb the wait, so the session lock
    // gets its own round to stay pinned.
    {
        let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            let mut caller_tx = writer_pool.begin().await.expect("caller tx");
            let picks = build_session_picks("10", "11");
            let session = plan_session(&["find-players"], &|_| true, &picks);
            let _ = record_session_routed_in_transaction(
                &mut caller_tx,
                GUILD,
                MEMBER_A,
                &session,
                "2026-10-13T00:00:00.000Z",
            )
            .await
            .expect("in-tx session write");
            caller_tx.commit().await.expect("caller commit");
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        opt_tx.commit().await.expect("opt-out commit");
        writer.await.expect("writer task");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows, 0,
            "in-transaction session routing racing the opt-out writes no row"
        );
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .execute(&pool)
            .await
            .unwrap();
    }

    db.close().await;
    test_db.close().await.unwrap();
}

/// `store_counters` deterministic race: one baseline write while the opt-out
/// holds its locks open. Without the writer's lock the gate passes and the
/// upsert lands after the purge with the suppressed inviter named; a single
/// write cannot be healed by a later iteration, so this pins the lock (the
/// looping test above is best-effort timing cover).
#[tokio::test]
async fn counter_deterministic_race_never_attributing() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        store_counters(
            &pool,
            GUILD,
            &[CounterRow {
                code: "race-single".to_owned(),
                uses: 7,
                inviter_id: Some(MEMBER_A.to_owned()),
                channel_id: None,
            }],
            "2026-10-13T00:00:00Z",
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    writer.await.expect("writer task").expect("writer ok");
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
        "single counter write racing the opt-out never attributes"
    );
    db.close().await;
    test_db.close().await.unwrap();
}

/// Gateway batch deterministic race: one batch while the opt-out holds its
/// locks open. Without the batch commit's lock the prefetched suppressed set
/// is stale and the event lands after the purge.
#[tokio::test]
async fn gateway_batch_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        let store = GatewaySessionStore::new(pool, GUILD.to_owned(), 0);
        let session = GatewaySession {
            session_id: "race-single".into(),
            sequence: 1,
            resume_url: "ws://race".into(),
            updated_at_ms: 1_760_000_000_000,
        };
        let batch = FunnelBatch {
            events: vec![FunnelEvent {
                guild_id: guild_u64(),
                member_id: Some(member_u64(MEMBER_A)),
                event_type: EventType::MemberJoin,
                occurred_at: "2026-10-13T00:00:00.000Z".to_owned(),
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
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    writer.await.expect("writer task");
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(events, 0, "single batch racing the opt-out never persists");
    db.close().await;
    test_db.close().await.unwrap();
}

/// `record_fact` deterministic race: one fact while the opt-out holds its
/// locks open. Without the fact lock the gate passes and the insert lands
/// after the purge.
#[tokio::test]
async fn fact_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let pool = db.pool().clone();
    let writer = tokio::spawn(async move {
        let fact = message_fact(MessageFactInput {
            guild_id: GUILD,
            message_id: "race-single",
            channel_id: "chan-1",
            channel_class: "human",
            actor: &ClassifyInput {
                guild_id: GUILD.to_owned(),
                actor_id: MEMBER_A.to_owned(),
                is_bot: false,
                webhook_id: None,
                is_staff_automation: false,
                is_raid: false,
                is_staging: false,
                is_test: false,
            },
            occurred_at: "2026-10-13T00:00:00.000Z",
            content_length: None,
            classification: Classification {
                classification: "eligible_human".to_owned(),
                classifier_version: "privacy-seams-v1".to_owned(),
                matched_rule: "fixture".to_owned(),
            },
        });
        let _ = record_fact(&pool, &fact).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    writer.await.expect("writer task");
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM community_facts WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(facts, 0, "single fact racing the opt-out never persists");
    db.close().await;
    test_db.close().await.unwrap();
}

/// Inactivity sweep gated on suppression: a suppressed quiet member is
/// skipped, and a sweep racing the opt-out never recreates the purged
/// `member_inactive` row.
#[tokio::test]
async fn inactivity_sweep_drops_suppressed_pairs() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let pool = db.pool();
    for member in [MEMBER_A, MEMBER_B] {
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at) VALUES ($1, $2, '2026-01-01T00:00:00.000Z'::timestamptz)",
        )
        .bind(GUILD)
        .bind(member)
        .execute(pool)
        .await
        .unwrap();
    }
    suppress(&db, GUILD, MEMBER_A).await;
    let outcome = run_sweep(pool, "2026-09-07T00:00:00.000Z", 14)
        .await
        .expect("sweep");
    assert!(
        outcome.flagged.iter().all(|f| f.member_id != MEMBER_A),
        "suppressed member not flagged"
    );
    assert!(
        outcome.flagged.iter().any(|f| f.member_id == MEMBER_B),
        "other quiet members still flagged"
    );
    let suppressed_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = 'member_inactive'",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        suppressed_events, 0,
        "no inactive event for the suppressed pair"
    );
    db.close().await;
    test_db.close().await.unwrap();
}

/// Inactivity sweep deterministic race: the sweep selects the member, then
/// the opt-out commits before the per-member write. Without the sweep's lock
/// the gate passes and the `member_inactive` event lands after the purge.
#[tokio::test]
async fn inactivity_sweep_deterministic_race_never_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at) VALUES ($1, $2, '2026-01-01T00:00:00.000Z'::timestamptz)",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .execute(db.pool())
    .await
    .unwrap();
    let opt_tx = hold_opt_out_open(db.pool(), GUILD, MEMBER_A).await;
    let pool = db.pool().clone();
    let writer =
        tokio::spawn(async move { run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    opt_tx.commit().await.expect("opt-out commit");
    let _ = writer.await.expect("writer task").expect("sweep ok");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = 'member_inactive'",
    )
    .bind(GUILD)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        events, 0,
        "sweep racing the opt-out never recreates the row"
    );
    db.close().await;
    test_db.close().await.unwrap();
}
