//! Protected acceptance: ownership replies describe the committed room, not
//! merely the actor's queued intent. The paired builder must not edit this file.
//! Only the persistence/Discord boundaries are doubles; command parsing,
//! authorization, actor scheduling, ownership decisions and replies are real.
#![cfg(test)]

use super::*;
use std::sync::atomic::AtomicUsize;

const ACK_BOUND: Duration = Duration::from_millis(2_000);
const TEST_GUARD: Duration = Duration::from_millis(2_100);

#[derive(Clone, Copy)]
enum CommitMode {
    Accept,
    RefuseRevision,
    Unavailable,
    Hold,
}

struct CommitControl {
    mode: CommitMode,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    accepted: AtomicUsize,
}

#[derive(Clone)]
struct ReceiptStore {
    inner: Arc<Store>,
    control: Arc<CommitControl>,
}

// Forward the real stateful fixture, including the recipient journal and its
// revision checks. Only update_ownership's external completion is controlled.
macro_rules! forward_store {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $out:ty;)*) => {
        $(async fn $name(&self $(, $arg: $ty)*) -> $out {
            self.inner.$name($($arg),*).await
        })*
    };
}

impl RoomPersistence for ReceiptStore {
    forward_store! {
        fn creators(&self, guild: u64) -> Result<Vec<CreatorChannel>, StoreError>;
        fn rooms(&self, guild: u64) -> Result<Vec<VoiceRoom>, StoreError>;
        fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError>;
        fn creator_for(&self, guild: u64, channel: u64) -> Result<Option<CreatorChannel>, StoreError>;
        fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError>;
        fn pending_owner_grants(&self, guild: u64) -> Result<Vec<u64>, StoreError>;
        fn prepare_owner_grants(&self, room: &VoiceRoom, previous: u64) -> Result<OwnerGrantIntent, StoreError>;
        fn custom_names(&self, guild: u64) -> Result<Vec<(u64, String)>, StoreError>;
        fn save_custom_name(&self, guild: u64, channel: u64, name: Option<&str>) -> Result<bool, StoreError>;
        fn access_controls(&self, guild: u64) -> Result<AccessControls, StoreError>;
        fn save_access_controls(&self, guild: u64, controls: &AccessControls) -> Result<(), StoreError>;
        fn logging_settings(&self, guild: u64) -> Result<LoggingSettings, StoreError>;
        fn save_logging_settings(&self, guild: u64, settings: &LoggingSettings) -> Result<(), StoreError>;
        fn config_snapshot(&self, guild: u64) -> Result<VoiceConfiguration, StoreError>;
        fn config_apply(&self, guild: u64, config: &VoiceConfiguration, expected: &VoiceConfiguration) -> Result<(), StoreError>;
        fn forget(&self, guild: u64, channel: u64) -> Result<(), StoreError>;
        fn claim_create(&self, guild: u64, user: u64, leaving: &[u64], config: &CreateAdmissionConfig, now_secs: i64) -> Result<CreateClaim, StoreError>;
        fn bind_create_channel(&self, guild: u64, reservation: &str, channel: u64) -> Result<(), StoreError>;
        fn orphaned_create_channels(&self, guild: u64) -> Result<Vec<(String, u64)>, StoreError>;
        fn persist_create(&self, reservation: &str, room: &VoiceRoom) -> Result<(), StoreError>;
        fn settle_create(&self, reservation: &str) -> Result<bool, StoreError>;
        fn companions(&self, guild: u64) -> Result<Vec<TextCompanion>, StoreError>;
        fn add_companion(&self, companion: &TextCompanion) -> Result<bool, StoreError>;
        fn remove_companion(&self, guild: u64, room: u64) -> Result<Option<TextCompanion>, StoreError>;
        fn record_kick_audit(&self, rows: &[KickAuditRow]) -> Result<(), StoreError>;
        fn privacy(&self, guild: u64) -> Result<BTreeMap<u64, PrivacyRecord>, StoreError>;
        fn save_privacy(&self, guild: u64, room: u64, record: &PrivacyRecord) -> Result<bool, StoreError>;
        fn join_grants(&self, guild: u64) -> Result<BTreeMap<u64, BTreeSet<u64>>, StoreError>;
        fn save_join_grant(&self, guild: u64, room: u64, member: u64) -> Result<JoinGrantIntent, StoreError>;
        fn remove_join_grant(&self, guild: u64, room: u64, member: u64) -> Result<(), StoreError>;
    }

    async fn update_ownership(&self, room: &VoiceRoom, revision: &str) -> Result<bool, StoreError> {
        self.control.started.notify_one();
        match self.control.mode {
            CommitMode::RefuseRevision => return Ok(false),
            CommitMode::Unavailable => return Err(StoreError::Unavailable),
            CommitMode::Hold => self.control.release.acquire().await.unwrap().forget(),
            CommitMode::Accept => {}
        }
        let accepted = self.inner.update_ownership(room, revision).await?;
        if accepted {
            self.control.accepted.fetch_add(1, Ordering::SeqCst);
        }
        Ok(accepted)
    }
}

struct Receipt {
    response: InteractionResponse,
    durable_owner: u64,
    accepted_commits: usize,
}

struct ReceiptReplies {
    store: ReceiptStore,
    completed: Mutex<Vec<Receipt>>,
}

impl ReceiptReplies {
    fn record(&self, response: InteractionResponse) {
        // Snapshot at the user-visible completion boundary, not after waiting
        // for a later timer tick that could conceal an optimistic reply.
        let durable_owner = self.store.inner.rooms.lock().unwrap()[&500].owner_id;
        self.completed.lock().unwrap().push(Receipt {
            response,
            durable_owner,
            accepted_commits: self.store.control.accepted.load(Ordering::SeqCst),
        });
    }
}

impl InteractionReplies for ReceiptReplies {
    async fn defer(&self, _: &Interaction) -> Result<(), RoomHttpError> {
        Ok(())
    }

    async fn complete(
        &self,
        _: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.record(response);
        Ok(())
    }

    async fn respond(
        &self,
        _: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.record(response);
        Ok(())
    }
}

async fn receipt_fixture(
    mode: CommitMode,
    reclaim: bool,
) -> (VoiceRuntime<ReceiptStore, Http>, ReceiptReplies) {
    let inner = Arc::new(Store::new(Trace::default()));
    let mut stored = room(500);
    if reclaim {
        stored.owner_id = 301;
    }
    inner.rooms.lock().unwrap().insert(500, stored);
    let store = ReceiptStore {
        inner,
        control: Arc::new(CommitControl {
            mode,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            accepted: AtomicUsize::new(0),
        }),
    };
    let factory = store.clone();
    let runtime = VoiceRuntime::new(
        move || (factory.clone(), Http::new(Trace::default())),
        Duration::from_millis(10),
        true,
    );
    let mut guild = command_snapshot();
    guild.channels.push(channel(500, 2, Some(CATEGORY)));
    guild.members = vec![
        VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        },
        VoiceMember {
            member_id: 301,
            channel_id: 500,
            bot: Some(false),
        },
    ];
    guild.member_roles.insert(MEMBER, vec![]);
    guild.member_roles.insert(301, vec![]);
    assert!(runtime.publish_snapshot(GUILD, guild));
    let status = tokio::time::timeout(TEST_GUARD, runtime.worker_status(GUILD))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.tracked_rooms, 1,
        "fixture must load before exercising a reply"
    );
    (
        runtime,
        ReceiptReplies {
            store,
            completed: Mutex::new(Vec::new()),
        },
    )
}

fn ownership_interaction(reclaim: bool, actor: u64) -> Interaction {
    let (name, options) = if reclaim {
        ("reclaim", vec![])
    } else {
        ("transfer", vec![user_option("member", 301)])
    };
    voice_interaction_as(Some(command_data(name, options)), None, actor)
}

async fn receipt_reply(
    runtime: &VoiceRuntime<ReceiptStore, Http>,
    replies: &ReceiptReplies,
    interaction: &Interaction,
) {
    let start = tokio::time::Instant::now();
    tokio::time::timeout(
        TEST_GUARD,
        VoiceResponder::respond_with(runtime, replies, interaction, None, None),
    )
    .await
    .expect("ownership completion must not outlive the two-second acknowledgement budget");
    assert!(
        start.elapsed() <= ACK_BOUND,
        "ownership wait exceeded the documented /limit budget"
    );
    assert_eq!(
        replies.completed.lock().unwrap().len(),
        1,
        "one completion per interaction"
    );
}

fn assert_finalizing(receipt: &Receipt) {
    let text = response_text(&receipt.response);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("record") && (lower.contains("finaliz") || lower.contains("finalis")),
        "uncommitted ownership needs recorded/finalizing copy: {text}"
    );
    assert!(
        !text.starts_with("Transferred")
            && !text.contains("owner of this room again")
            && !text.contains("yours now"),
        "no committed-success copy before Ok(true): {text}"
    );
    assert!(
        [
            "try again",
            "retry",
            "check",
            "wait",
            "/reclaim",
            "/transfer"
        ]
        .iter()
        .any(|next| lower.contains(next)),
        "finalizing copy must offer a next action: {text}"
    );
    assert_eq!(receipt.accepted_commits, 0);
    assert_eq!(
        receipt.response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
}

#[tokio::test(start_paused = true)]
async fn transfer_success_observes_accepted_commit_at_reply_boundary() {
    let (runtime, replies) = receipt_fixture(CommitMode::Accept, false).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(false, MEMBER)).await;
    let receipts = replies.completed.lock().unwrap();
    let text = response_text(&receipts[0].response);
    assert!(
        text.contains("Transferred") && text.contains("<@301>"),
        "{text}"
    );
    assert_eq!(
        receipts[0].durable_owner, 301,
        "the reply must not precede commit"
    );
    assert!(
        receipts[0].accepted_commits > 0,
        "only Ok(true) authorizes success"
    );
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn reclaim_success_observes_accepted_commit_at_reply_boundary() {
    let (runtime, replies) = receipt_fixture(CommitMode::Accept, true).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(true, MEMBER)).await;
    let receipts = replies.completed.lock().unwrap();
    let text = response_text(&receipts[0].response);
    assert!(
        text.contains("owner") && !text.contains("finaliz"),
        "{text}"
    );
    assert_eq!(
        receipts[0].durable_owner, MEMBER,
        "reclaim must commit before success"
    );
    assert!(receipts[0].accepted_commits > 0);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn transfer_store_error_is_finalizing_not_success() {
    let (runtime, replies) = receipt_fixture(CommitMode::Unavailable, false).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(false, MEMBER)).await;
    let receipts = replies.completed.lock().unwrap();
    assert_finalizing(&receipts[0]);
    assert_eq!(receipts[0].durable_owner, MEMBER);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn reclaim_store_error_is_finalizing_not_success() {
    let (runtime, replies) = receipt_fixture(CommitMode::Unavailable, true).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(true, MEMBER)).await;
    let receipts = replies.completed.lock().unwrap();
    assert_finalizing(&receipts[0]);
    assert_eq!(receipts[0].durable_owner, 301);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn superseded_revision_is_not_an_accepted_commit() {
    let (runtime, replies) = receipt_fixture(CommitMode::RefuseRevision, false).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(false, MEMBER)).await;
    assert_finalizing(&replies.completed.lock().unwrap()[0]);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn stalled_commit_finishes_with_pending_copy_within_budget() {
    let (runtime, replies) = receipt_fixture(CommitMode::Hold, false).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(false, MEMBER)).await;
    assert_finalizing(&replies.completed.lock().unwrap()[0]);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn reclaim_refused_revision_is_finalizing_not_success() {
    let (runtime, replies) = receipt_fixture(CommitMode::RefuseRevision, true).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(true, MEMBER)).await;
    assert_finalizing(&replies.completed.lock().unwrap()[0]);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn reclaim_stalled_commit_finishes_with_pending_copy_within_budget() {
    let (runtime, replies) = receipt_fixture(CommitMode::Hold, true).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(true, MEMBER)).await;
    assert_finalizing(&replies.completed.lock().unwrap()[0]);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn a_commit_released_inside_the_budget_can_report_success() {
    let (runtime, replies) = receipt_fixture(CommitMode::Hold, false).await;
    let interaction = ownership_interaction(false, MEMBER);
    let release = async {
        replies.store.control.started.notified().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        replies.store.control.release.add_permits(1);
    };
    tokio::time::timeout(TEST_GUARD, async {
        tokio::join!(receipt_reply(&runtime, &replies, &interaction), release);
    })
    .await
    .expect("held commit must start and complete within budget");
    let receipts = replies.completed.lock().unwrap();
    assert!(response_text(&receipts[0].response).contains("Transferred"));
    assert_eq!(receipts[0].durable_owner, 301);
    assert!(receipts[0].accepted_commits > 0);
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn admin_transfer_outside_voice_identifies_the_selected_room() {
    let (runtime, replies) = receipt_fixture(CommitMode::Accept, false).await;
    let mut interaction = ownership_interaction(false, 998); // authoritative guild owner
    interaction.member.as_mut().unwrap().roles.clear();
    receipt_reply(&runtime, &replies, &interaction).await;
    let receipts = replies.completed.lock().unwrap();
    let text = response_text(&receipts[0].response);
    assert!(
        text.contains("<#500>") && text.contains("<@301>"),
        "remote admin copy must name the affected room: {text}"
    );
    assert_eq!(receipts[0].durable_owner, 301);
    assert!(receipts[0].accepted_commits > 0);
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn admin_in_another_tracked_room_keeps_own_room_precedence() {
    // Chosen permitted resolution: preserve this behavior and narrow the V2
    // documentation. A target-room override must not silently change policy.
    let (live, store, http, _) = owned_room();
    store.rooms.lock().unwrap().insert(600, room(600));
    live.upsert_channel(channel(600, 2, Some(CATEGORY)));
    live.voice_update_at(998, Some(600), Some(false), 1_000);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let text = worker.apply_ownership(998, true, OwnershipCommand::Transfer { target_id: 301 });
    assert!(text.contains("must be in the room"), "{text}");
    assert_eq!(worker.tracked()[&500].owner_id, MEMBER);
    assert_eq!(worker.tracked()[&600].owner_id, MEMBER);
    assert_eq!(worker.store.rooms.lock().unwrap()[&500].owner_id, MEMBER);
}

#[tokio::test(start_paused = true)]
async fn immediate_followup_transfer_does_not_add_an_ownership_cooldown() {
    let (runtime, replies) = receipt_fixture(CommitMode::Accept, false).await;
    receipt_reply(&runtime, &replies, &ownership_interaction(false, MEMBER)).await;
    replies.completed.lock().unwrap().clear();
    let interaction = voice_interaction_as(
        Some(command_data(
            "transfer",
            vec![user_option("member", MEMBER)],
        )),
        None,
        301,
    );
    receipt_reply(&runtime, &replies, &interaction).await;
    let receipts = replies.completed.lock().unwrap();
    assert!(response_text(&receipts[0].response).contains("Transferred"));
    assert_eq!(receipts[0].durable_owner, MEMBER);
    assert_eq!(
        replies.store.inner.rooms.lock().unwrap()[&500].original_creator_id,
        MEMBER
    );
    runtime.remove_guild(GUILD);
}

#[tokio::test(start_paused = true)]
async fn ownership_refusals_still_have_no_durable_or_grant_side_effects() {
    for (reclaim, actor, target, bot, channel_permission) in [
        (true, 301, 0, false, false), // non-creator cannot reclaim from present owner
        (false, 301, MEMBER, false, true), // channel grants do not make an admin
        (false, MEMBER, 302, false, false), // absent recipient
        (false, MEMBER, 301, true, false), // bot recipient
    ] {
        let (runtime, replies) = receipt_fixture(CommitMode::Accept, false).await;
        if bot {
            runtime.live_actor(GUILD).unwrap().live.voice_update_at(
                301,
                Some(500),
                Some(true),
                1_000,
            );
        }
        let mut interaction = if reclaim {
            ownership_interaction(true, actor)
        } else {
            voice_interaction_as(
                Some(command_data(
                    "transfer",
                    vec![user_option("member", target)],
                )),
                None,
                actor,
            )
        };
        if channel_permission {
            let member = interaction.member.as_mut().unwrap();
            member.permissions = Some(Permissions::MANAGE_CHANNELS);
            member.roles.clear();
        }
        receipt_reply(&runtime, &replies, &interaction).await;
        let receipts = replies.completed.lock().unwrap();
        let text = response_text(&receipts[0].response);
        let expected = if reclaim {
            "original creator"
        } else if bot {
            "Bots can't own"
        } else if channel_permission {
            "Only the room owner"
        } else {
            "must be in the room"
        };
        assert!(text.contains(expected), "{text}");
        assert_eq!(receipts[0].durable_owner, MEMBER);
        assert_eq!(receipts[0].accepted_commits, 0);
        assert!(replies.store.inner.owner_intents.lock().unwrap().is_empty());
        runtime.remove_guild(GUILD);
    }
}

#[tokio::test]
async fn succession_logs_previous_and_next_owner_once_at_info_with_bounded_counter() {
    let (live, store, http, _) = owned_room();
    // 302 arrived after 301, and the bot arrived first: neither may displace
    // the longest-present human when the owner leaves.
    live.voice_update_at(301, None, Some(false), 50);
    live.voice_update_at(303, Some(500), Some(true), 100);
    live.voice_update_at(301, Some(500), Some(false), 200);
    live.voice_update_at(302, Some(500), Some(false), 300);
    let mut worker = GuildRoomWorker::load(live.clone(), store, http)
        .await
        .unwrap();
    live.voice_update_at(MEMBER, None, Some(false), 400);
    tracing_capture::global();
    let capture = tracing_capture::Capture::default();
    let metric = "two_bot_voice_reconcile_actions_total{action=\"succession_enqueued\"}";
    let before = global_series(metric);
    tracing::subscriber::with_default(capture.clone(), || {
        worker.reconcile();
        worker.reconcile();
        worker.reconcile();
    });
    assert_eq!(worker.tracked()[&500].owner_id, 301);
    assert_eq!(worker.tracked()[&500].original_creator_id, MEMBER);
    let text = capture.text();
    let transitions: Vec<_> = text
        .lines()
        .filter(|line| line.contains("previous_owner_id=300") && line.contains("next_owner_id=301"))
        .collect();
    assert_eq!(
        transitions.len(),
        1,
        "one structured owner transition, not only an aggregate: {text}"
    );
    assert!(
        transitions[0].starts_with("INFO "),
        "succession is an info-level lifecycle event: {text}"
    );
    assert!(
        global_series(metric) > before,
        "succession must increment its fixed-vocabulary counter"
    );
    let rendered = metrics::global().render(None);
    for line in rendered
        .lines()
        .filter(|line| line.starts_with("two_bot_voice_reconcile_actions_total{"))
    {
        assert!(
            !line.contains("owner_id=")
                && !line.contains("guild_id=")
                && !line.contains("channel_id="),
            "identities belong in logs, not metric labels: {line}"
        );
    }
}
