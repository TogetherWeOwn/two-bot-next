//! Protected RED-first acceptance for persistent, evidence-only voice orphans.
//!
//! The builder preserves this file. Only the existing `/create` failed-save,
//! failed-compensation path produces this kind of orphan. Ordinary room
//! compensation retains provenance; tracked ghosts remain reconciliation work.
//!
//! Storage contract: `voice_orphan_ledger(guild_id text, channel_id text,
//! reason text, recorded_at timestamptz)`, unique by (guild_id, channel_id),
//! at most 1000 rows per guild, oldest recorded_at evicted first. Re-observing
//! an existing channel retains its original evidence. Reason is the bounded
//! code `creator_compensation_failed`, not a driver error or user input.
//!
//! Read contract: `/voice-orphans [page:integer]`, default 1, ten whole rows
//! per page, newest first, deterministic channel-id tie break. Each row shows
//! its channel mention, reason and RFC3339 time; reply names the guild and
//! `Page n of total`, with a copyable next-page invocation. Manage Channels
//! or Administrator from trusted guild facts is required BEFORE any store
//! construction/read (including the common access-controls gate). All replies
//! are private and mention-safe. Invalid pages refuse, never silently clamp.
//!
//! Only disposable TestDatabase fixtures and the existing Discord Http double
//! are used. No ignored tests and no missing-URL success. A missing table is
//! an explicit assertion RED, not a compile error. No production fallback or
//! test-local ledger implementation can satisfy these tests.

#![cfg(test)]

use super::*;
use futures_util::FutureExt;
use sqlx::PgPool;
use std::ops::AsyncFnOnce;
use std::panic::{resume_unwind, AssertUnwindSafe};
use two_bot_testsupport::TestDatabase;

#[path = "../../core/tests/support/hotpath_queries.rs"]
mod hotpath_queries;
use hotpath_queries::Queries;

const CAP: i64 = 1000;
const PAGE_SIZE: usize = 10;
const REASON: &str = "creator_compensation_failed";
const SEED_TIME: i64 = 1_700_000_000;
const FIRST_SEED: u64 = 8_000_000_000_000_000;
const FOREIGN: u64 = 9_000_000_000_000_000;

type Row = (String, String, String, OffsetDateTime);

async fn with_database(check: impl for<'a> AsyncFnOnce(&'a TestDatabase)) {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable TWO_TEST_DATABASE_URL required; acceptance must not skip");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create isolated migrated test database");
    // A deliberately RED assertion must still finish explicit fixture teardown.
    let outcome = AssertUnwindSafe(check(&db)).catch_unwind().await;
    db.close()
        .await
        .expect("drop only the owned disposable database");
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
}

async fn ledger_exists(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.voice_orphan_ledger') IS NOT NULL")
        .fetch_one(pool)
        .await
        .expect("inspect fixture schema")
}

async fn require_ledger(pool: &PgPool) {
    assert!(
        ledger_exists(pool).await,
        "missing persistent voice_orphan_ledger: manual-cleanup channel IDs currently die with the reply"
    );
}

async fn rows(pool: &PgPool) -> Vec<Row> {
    // Baseline controls can prove non-interference before the ledger exists.
    if !ledger_exists(pool).await {
        return Vec::new();
    }
    sqlx::query_as(
        "SELECT guild_id, channel_id, reason, recorded_at FROM voice_orphan_ledger
         ORDER BY guild_id, recorded_at, channel_id",
    )
    .fetch_all(pool)
    .await
    .expect("read complete ledger evidence through an independent SQL boundary")
}

async fn reject_creator_writes(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION reject_creator_fixture() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic creator write failure'; END $$;
         CREATE TRIGGER reject_creator_fixture BEFORE INSERT OR UPDATE ON voice_creators
         FOR EACH ROW EXECUTE FUNCTION reject_creator_fixture()",
    )
    .execute(pool)
    .await
    .expect("install fixture-only creator failure without breaking other tables");
}

async fn seed(pool: &PgPool, guild: u64, first: u64, count: i64) {
    sqlx::query(
        "INSERT INTO voice_orphan_ledger (guild_id, channel_id, reason, recorded_at)
         SELECT $1, ($2 + n)::text, $3, to_timestamp($4 + n)
         FROM generate_series(0::bigint, $5 - 1) AS n",
    )
    .bind(guild.to_string())
    .bind(i64::try_from(first).unwrap())
    .bind(REASON)
    .bind(SEED_TIME)
    .bind(count)
    .execute(pool)
    .await
    .expect("seed independently enumerated evidence rows");
}

struct Harness {
    runtime: VoiceRuntime<PgRoomStore, Http>,
    trace: Trace,
    store_constructions: Arc<AtomicU64>,
    next_channel: Arc<AtomicU64>,
    delete_error: Arc<Mutex<Option<RoomHttpError>>>,
    create_error: Arc<Mutex<Option<RoomHttpError>>>,
    // Trusted facts without an independently spawned actor doing hidden reads.
    _inbox: mpsc::UnboundedReceiver<ActorCommand>,
}

impl Harness {
    fn new(pool: PgPool) -> Self {
        let trace = Trace::default();
        let store_constructions = Arc::new(AtomicU64::new(0));
        let next_channel = Arc::new(AtomicU64::new(500));
        let delete_error = Arc::new(Mutex::new(None));
        let create_error = Arc::new(Mutex::new(None));
        let runtime = VoiceRuntime::new(
            {
                let trace = trace.clone();
                let calls = store_constructions.clone();
                let channel = next_channel.clone();
                let deletion = delete_error.clone();
                let creation = create_error.clone();
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let http = Http::new(trace.clone());
                    *http.next_id.lock().unwrap() = channel.load(Ordering::SeqCst);
                    if let Some(error) = *deletion.lock().unwrap() {
                        http.delete_errors.lock().unwrap().push_back(error);
                    }
                    if let Some(error) = *creation.lock().unwrap() {
                        http.create_errors.lock().unwrap().push_back(error);
                    }
                    (PgRoomStore::new(pool.clone()), http)
                }
            },
            Duration::from_millis(250),
            true,
        );
        let live = LiveGuild::new(GUILD);
        live.publish(command_snapshot());
        let (tx, inbox) = mpsc::unbounded_channel();
        runtime.actors.lock().unwrap().insert(
            GUILD,
            GuildActor {
                live,
                tx,
                backpressure: ActorInboxBackpressure::default(),
            },
        );
        Self {
            runtime,
            trace,
            store_constructions,
            next_channel,
            delete_error,
            create_error,
            _inbox: inbox,
        }
    }

    async fn call(&self, interaction: &Interaction) -> InteractionResponse {
        let (owned, response) = tokio::time::timeout(
            Duration::from_secs(5),
            handle_capture(&self.runtime, interaction),
        )
        .await
        .expect("read/create dispatch must not wait on a guild worker");
        assert!(owned, "voice runtime must own the forged command");
        let response = response.expect("exactly one command reply");
        assert_eq!(
            response.kind,
            InteractionResponseType::ChannelMessageWithSource
        );
        let data = response.data.as_ref().expect("private response data");
        assert!(data
            .flags
            .unwrap_or_default()
            .contains(MessageFlags::EPHEMERAL));
        let text = response_text(&response);
        assert!(two_bot_core::message_safety::text_len(&text) <= 2000);
        assert!(!text.contains("@everyone") && !text.contains("@here"));
        response
    }

    async fn failed_create(&self, id: u64, error: RoomHttpError) -> String {
        self.next_channel.store(id, Ordering::SeqCst);
        *self.delete_error.lock().unwrap() = Some(error);
        response_text(&self.call(&create_interaction()).await)
    }
}

fn create_interaction() -> Interaction {
    voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

fn read_interaction(
    permissions: Option<Permissions>,
    options: Vec<CommandDataOption>,
) -> Interaction {
    voice_interaction(
        Some(command_data("voice-orphans", options)),
        permissions,
        true,
    )
}

fn page(value: i64) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value: CommandOptionValue::Integer(value),
    }
}

fn mentions(text: &str) -> Vec<u64> {
    text.split("<#")
        .skip(1)
        .map(|tail| {
            tail.split_once('>')
                .expect("whole channel mention")
                .0
                .parse()
                .unwrap()
        })
        .collect()
}

fn stamp(offset: i64) -> String {
    OffsetDateTime::from_unix_timestamp(SEED_TIME + offset)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[test]
fn staff_read_is_in_the_voice_registry_with_a_positive_optional_page() {
    let defs = voice_command_set(&VoiceGates { enabled: true });
    let command = defs
        .iter()
        .find(|def| def.name == "voice-orphans")
        .expect("missing staff ledger read command");
    assert_eq!(
        command.options.len(),
        1,
        "read-only page surface, no deletion option"
    );
    let option = &command.options[0];
    assert_eq!(option.name, "page");
    assert_eq!(
        option.kind,
        two_bot_core::commands::CommandOptionType::Integer.as_u8()
    );
    assert_ne!(option.required, Some(true));
    assert_eq!(option.min_value, Some(1));
    assert!(!voice_command_set(&VoiceGates { enabled: false })
        .iter()
        .any(|def| def.name == "voice-orphans"));
}

#[tokio::test]
async fn every_failed_creator_compensation_records_exact_identity_reason_and_time() {
    with_database(async |db| {
        reject_creator_writes(db.pool()).await;
        let harness = Harness::new(db.pool().clone());
        let before = OffsetDateTime::now_utc();
        for (index, error) in [
            RoomHttpError::AccessDenied,
            RoomHttpError::Unauthorized,
            RoomHttpError::UnknownOutcome,
            RoomHttpError::RateLimited { retry_after_ms: 1000, global: false },
            RoomHttpError::Rejected { status: 500, code: 0 },
        ].into_iter().enumerate() {
            let id = 500 + index as u64;
            let text = harness.failed_create(id, error).await;
            assert_eq!(text, format!("Could not save the new creator channel, and removing it failed. Delete <#{id}> manually and try again."));
        }
        assert_eq!(*harness.trace.lock().unwrap(), [
            "create", "delete:500", "create", "delete:501", "create", "delete:502",
            "create", "delete:503", "create", "delete:504",
        ]);
        require_ledger(db.pool()).await;
        let evidence = rows(db.pool()).await;
        assert_eq!(evidence.len(), 5, "exactly one row per untracked channel");
        let after = OffsetDateTime::now_utc();
        for id in 500..505 {
            let matching: Vec<_> = evidence.iter().filter(|row| row.1 == id.to_string()).collect();
            assert_eq!(matching.len(), 1);
            let row = matching[0];
            assert_eq!(row.0, GUILD.to_string());
            assert_eq!(row.2, REASON, "never persist names or raw errors");
            assert!(row.3 >= before && row.3 <= after, "observation time, not a fixed sentinel");
        }
        assert!(PgRoomStore::new(db.pool().clone()).creators(GUILD).await.unwrap().is_empty());
        assert!(PgRoomStore::new(db.pool().clone()).rooms_in_guild(GUILD).await.unwrap().is_empty());
    }).await;
}

#[tokio::test]
async fn duplicate_observations_and_restart_preserve_one_immutable_evidence_row() {
    with_database(async |db| {
        reject_creator_writes(db.pool()).await;
        let harness = Harness::new(db.pool().clone());
        harness
            .failed_create(500, RoomHttpError::AccessDenied)
            .await;
        require_ledger(db.pool()).await;
        let first = rows(db.pool()).await;
        assert_eq!(first.len(), 1);
        harness
            .failed_create(500, RoomHttpError::UnknownOutcome)
            .await;
        assert_eq!(
            rows(db.pool()).await,
            first,
            "duplicates cannot refresh retention or overwrite evidence"
        );
        drop(harness);
        let peer = db.independent_pool().await.unwrap();
        let restarted = Harness::new(peer.clone());
        restarted
            .failed_create(500, RoomHttpError::Unauthorized)
            .await;
        assert_eq!(
            rows(&peer).await,
            first,
            "fresh runtime and independent connections see identical evidence"
        );
        let response = restarted
            .call(&read_interaction(
                Some(Permissions::MANAGE_CHANNELS),
                vec![],
            ))
            .await;
        assert_eq!(
            mentions(&response_text(&response)),
            [500],
            "read must use durable rows, not a process cache"
        );
        drop(restarted);
        peer.close().await;
    })
    .await;
}

#[tokio::test]
async fn cap_evicts_only_oldest_in_the_same_guild_and_duplicate_does_not_shrink_it() {
    with_database(async |db| {
        require_ledger(db.pool()).await;
        seed(db.pool(), GUILD, FIRST_SEED, CAP).await;
        seed(db.pool(), GUILD + 1, FOREIGN, 2).await;
        let foreign: Vec<_> = rows(db.pool())
            .await
            .into_iter()
            .filter(|row| row.0 != GUILD.to_string())
            .collect();
        reject_creator_writes(db.pool()).await;
        let harness = Harness::new(db.pool().clone());
        for id in 500..503 {
            harness.failed_create(id, RoomHttpError::AccessDenied).await;
            let evidence = rows(db.pool()).await;
            assert_eq!(
                evidence
                    .iter()
                    .filter(|row| row.0 == GUILD.to_string())
                    .count(),
                CAP as usize
            );
        }
        let evidence = rows(db.pool()).await;
        let ids: BTreeSet<_> = evidence
            .iter()
            .filter(|row| row.0 == GUILD.to_string())
            .map(|row| row.1.parse::<u64>().unwrap())
            .collect();
        let expected: BTreeSet<_> = (FIRST_SEED + 3..FIRST_SEED + CAP as u64)
            .chain(500..503)
            .collect();
        assert_eq!(
            ids, expected,
            "newest retained, exactly the three oldest evicted"
        );
        assert_eq!(
            evidence
                .iter()
                .filter(|row| row.0 != GUILD.to_string())
                .cloned()
                .collect::<Vec<_>>(),
            foreign
        );
        harness
            .failed_create(502, RoomHttpError::UnknownOutcome)
            .await;
        assert_eq!(
            rows(db.pool()).await,
            evidence,
            "a duplicate cannot evict a fourth row"
        );
    })
    .await;
}

#[tokio::test]
async fn concurrent_observations_keep_the_cap_and_database_identity_is_unique() {
    with_database(async |db| {
        require_ledger(db.pool()).await;
        seed(db.pool(), GUILD, FIRST_SEED, CAP).await;
        reject_creator_writes(db.pool()).await;
        let first = Harness::new(db.pool().clone());
        let second = Harness::new(db.pool().clone());
        let third = Harness::new(db.pool().clone());
        let fourth = Harness::new(db.pool().clone());
        tokio::join!(
            first.failed_create(500, RoomHttpError::AccessDenied),
            second.failed_create(501, RoomHttpError::AccessDenied),
            third.failed_create(502, RoomHttpError::AccessDenied),
            fourth.failed_create(500, RoomHttpError::UnknownOutcome),
        );
        let evidence = rows(db.pool()).await;
        assert_eq!(
            evidence.len(),
            CAP as usize,
            "parallel append/trim cannot exceed the cap"
        );
        let ids: BTreeSet<_> = evidence
            .iter()
            .map(|row| row.1.parse::<u64>().unwrap())
            .collect();
        let expected: BTreeSet<_> = (FIRST_SEED + 3..FIRST_SEED + CAP as u64)
            .chain(500..503)
            .collect();
        assert_eq!(
            ids, expected,
            "three distinct new channels, not four observations"
        );
        let duplicate = sqlx::query(
            "INSERT INTO voice_orphan_ledger (guild_id, channel_id, reason, recorded_at)
             VALUES ('100', '500', 'creator_compensation_failed', now())",
        )
        .execute(db.pool())
        .await
        .expect_err("the database itself must reject duplicate identity");
        assert_eq!(
            duplicate
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("23505")
        );
        assert_eq!(rows(db.pool()).await, evidence);
    })
    .await;
}

#[tokio::test]
async fn private_staff_pages_cover_each_guild_row_once_and_are_restart_stable() {
    with_database(async |db| {
        require_ledger(db.pool()).await;
        seed(db.pool(), GUILD, FIRST_SEED, 23).await;
        seed(db.pool(), GUILD + 1, FOREIGN, 2).await;
        let before = rows(db.pool()).await;
        let harness = Harness::new(db.pool().clone());
        let mut all = Vec::new();
        for number in 1..=3 {
            let options = if number == 1 {
                vec![]
            } else {
                vec![page(number)]
            };
            let request = read_interaction(Some(Permissions::MANAGE_CHANNELS), options);
            let response = harness.call(&request).await;
            let text = response_text(&response);
            assert!(text.contains(&format!("Page {number} of 3")), "{text}");
            assert!(
                text.contains(&GUILD.to_string()),
                "guild context is explicit: {text}"
            );
            let ids = mentions(&text);
            let end = (number as usize * PAGE_SIZE).min(23);
            let start = (number as usize - 1) * PAGE_SIZE;
            let expected: Vec<_> = (start..end)
                .map(|index| FIRST_SEED + 22 - index as u64)
                .collect();
            assert_eq!(ids, expected, "whole rows in newest-first order: {text}");
            for id in &ids {
                let line = text
                    .lines()
                    .find(|line| line.contains(&format!("<#{id}>")))
                    .unwrap();
                assert!(
                    line.contains(REASON),
                    "reason belongs with its channel: {line}"
                );
                assert!(
                    line.contains(&stamp((*id - FIRST_SEED) as i64)),
                    "recorded time belongs with its channel: {line}"
                );
            }
            if number < 3 {
                assert!(
                    text.contains(&format!("/voice-orphans page:{}", number + 1)),
                    "copyable continuation: {text}"
                );
            }
            assert_eq!(
                serde_json::to_vec(&harness.call(&request).await).unwrap(),
                serde_json::to_vec(&response).unwrap()
            );
            all.extend(ids);
        }
        assert_eq!(all.len(), 23);
        assert_eq!(all.iter().copied().collect::<BTreeSet<_>>().len(), 23);
        let peer = db.independent_pool().await.unwrap();
        let restarted = Harness::new(peer.clone());
        let request = read_interaction(Some(Permissions::ADMINISTRATOR), vec![page(2)]);
        assert_eq!(
            response_text(&restarted.call(&request).await),
            response_text(&harness.call(&request).await)
        );
        assert!(
            harness.trace.lock().unwrap().is_empty(),
            "reading evidence never writes Discord"
        );
        assert_eq!(
            rows(db.pool()).await,
            before,
            "paging never mutates evidence"
        );
        drop(restarted);
        peer.close().await;
    })
    .await;
}

#[tokio::test]
async fn non_staff_and_forged_permission_claims_refuse_before_any_store_access() {
    with_database(async |db| {
        // Prove the SQL observer sees the actual pool before trusting an empty capture.
        let (probe, receipts) =
            Queries::capture(sqlx::query_scalar::<_, i32>("SELECT 42").fetch_one(db.pool())).await;
        assert_eq!(probe.unwrap(), 42);
        assert!(receipts.iter().any(|receipt| receipt.rows == 1
            && receipt
                .normalized()
                .contains("SELECT 42".to_ascii_lowercase().as_str())));
        let harness = Harness::new(db.pool().clone());
        let mut forged = read_interaction(Some(Permissions::ADMINISTRATOR), vec![page(0)]);
        forged.member.as_mut().unwrap().roles.clear();
        let mut missing_member = read_interaction(None, vec![]);
        missing_member.member = None;
        for request in [
            read_interaction(Some(Permissions::empty()), vec![]),
            read_interaction(Some(Permissions::MANAGE_GUILD), vec![page(2)]),
            read_interaction(None, vec![page(0)]),
            forged,
            missing_member,
        ] {
            let before = harness.store_constructions.load(Ordering::SeqCst);
            let (response, receipts) = Queries::capture(harness.call(&request)).await;
            let text = response_text(&response);
            assert!(
                text.contains("Manage Channels"),
                "authority refusal, not page validation or an empty ledger: {text}"
            );
            assert!(
                !text.contains("Page ") && mentions(&text).is_empty() && !text.contains(REASON)
            );
            assert_eq!(
                harness.store_constructions.load(Ordering::SeqCst),
                before,
                "refuse before even constructing a store, including the shared command gate"
            );
            assert!(
                receipts.is_empty(),
                "denial executed database work: {receipts:?}"
            );
        }
        assert!(harness.trace.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn malformed_and_past_end_staff_pages_refuse_without_mutating_or_exposing_rows() {
    with_database(async |db| {
        require_ledger(db.pool()).await;
        seed(db.pool(), GUILD, FIRST_SEED, 11).await;
        let before = rows(db.pool()).await;
        let harness = Harness::new(db.pool().clone());
        for options in [
            vec![page(0)],
            vec![page(-1)],
            vec![page(i64::MAX)],
            vec![page(1), page(2)],
            vec![command_option("page", "1")],
            vec![page(3)],
        ] {
            let response = harness
                .call(&read_interaction(
                    Some(Permissions::MANAGE_CHANNELS),
                    options,
                ))
                .await;
            let text = response_text(&response);
            assert!(
                text.to_ascii_lowercase().contains("page")
                    && text.contains("/voice-orphans")
                    && text.contains('1'),
                "actionable page refusal: {text}"
            );
            assert!(
                mentions(&text).is_empty(),
                "invalid page is never clamped to a data page: {text}"
            );
            assert!(two_bot_core::message_safety::text_len(&text) <= 500);
        }
        assert_eq!(rows(db.pool()).await, before);
        assert!(harness.trace.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn control_successful_creator_and_successful_compensation_keep_identical_replies() {
    with_database(async |db| {
        let harness = Harness::new(db.pool().clone());
        let response = harness.call(&create_interaction()).await;
        assert_eq!(
            serde_json::to_vec(&response).unwrap(),
            serde_json::to_vec(&ephemeral_response(
                "Created <#500>: join it to spin up temporary voice rooms."
            ))
            .unwrap()
        );
        assert_eq!(*harness.trace.lock().unwrap(), ["create"]);
        assert_eq!(
            PgRoomStore::new(db.pool().clone())
                .creators(GUILD)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(rows(db.pool()).await.is_empty());
        reject_creator_writes(db.pool()).await;
        harness.next_channel.store(501, Ordering::SeqCst);
        let response = harness.call(&create_interaction()).await;
        assert_eq!(
            serde_json::to_vec(&response).unwrap(),
            serde_json::to_vec(&ephemeral_response(
                "Could not save the new creator channel, so it was removed. Try again."
            ))
            .unwrap()
        );
        assert_eq!(
            *harness.trace.lock().unwrap(),
            ["create", "create", "delete:501"]
        );
        assert!(
            rows(db.pool()).await.is_empty(),
            "successful compensation is not an orphan"
        );
    })
    .await;
}

#[tokio::test]
async fn control_unknown_create_receipt_invents_no_channel_and_writes_no_ledger() {
    with_database(async |db| {
        let harness = Harness::new(db.pool().clone());
        *harness.create_error.lock().unwrap() = Some(RoomHttpError::UnknownOutcome);
        let response = harness.call(&create_interaction()).await;
        assert!(
            mentions(&response_text(&response)).is_empty(),
            "no trusted returned channel ID"
        );
        assert_eq!(*harness.trace.lock().unwrap(), ["create"]);
        assert!(rows(db.pool()).await.is_empty());
    })
    .await;
}

#[tokio::test]
async fn control_real_reconcile_drops_tracked_ghost_but_never_touches_untracked_residue() {
    with_database(async |db| {
        let store = PgRoomStore::new(db.pool().clone());
        store
            .add_creator(&CreatorChannel::new(GUILD, CREATOR))
            .await
            .unwrap();
        assert!(store.add_room(&room(500)).await.unwrap());
        let live = LiveGuild::new(GUILD);
        // 500 is the tracked ghost; 777 is present but has no owned room row.
        live.publish(snapshot(&[777], vec![]));
        live.set_empty_grace(Duration::ZERO);
        let trace = Trace::default();
        let mut worker = GuildRoomWorker::load(live, store.clone(), Http::new(trace.clone()))
            .await
            .unwrap();
        worker.reconcile();
        assert!(worker.dispatch_one(60_000).await);
        assert!(worker.tracked().is_empty());
        assert!(store.rooms_in_guild(GUILD).await.unwrap().is_empty());
        assert!(
            trace.lock().unwrap().is_empty(),
            "ghost has no REST delete; untracked channel has no delete authority"
        );
        assert!(worker
            .live
            .inner
            .read()
            .unwrap()
            .channels
            .contains_key(&777));
        for now in [120_000, 180_000] {
            worker.reconcile();
            assert!(!worker.dispatch_one(now).await);
        }
        assert!(
            rows(db.pool()).await.is_empty(),
            "tracked cleanup and arbitrary inventory are not failed-compensation evidence"
        );
    })
    .await;
}

#[tokio::test]
async fn control_failed_room_rollback_keeps_its_existing_tracked_compensation_witness() {
    let (live, mut store, http, trace) = fixture();
    store.persist_error = Some(StoreError::Unavailable);
    http.delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500"]
    );
    assert_eq!(worker.tracked()[&500].original_creator_id, MEMBER);
    assert!(worker.compensation.contains(&500));
    assert_eq!(worker.store.bound.lock().unwrap().len(), 1);
    for now in [3000, 6000, 10_000] {
        worker.reconcile();
        assert!(!worker.dispatch_one(now).await);
    }
    assert_eq!(
        *trace.lock().unwrap(),
        ["create", "persist:500", "delete:500"]
    );
}
