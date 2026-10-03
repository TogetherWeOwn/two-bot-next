use super::*;
use two_bot_core::voice_vote_kick::{VoteCancellation, VOTE_KICK_TTL_MS};

const OWNER: u64 = MEMBER;
const VOTER_A: u64 = 301;
const VOTER_B: u64 = 302;
const TARGET: u64 = 303;
const VOTER_C: u64 = 304;
const ROOM: u64 = 500;
const VOTE: u64 = 7_000;

fn occupant(member_id: u64) -> VoiceMember {
    VoiceMember {
        member_id,
        channel_id: ROOM,
        bot: Some(false),
    }
}

fn roster() -> Vec<VoiceMember> {
    [OWNER, VOTER_A, VOTER_B, TARGET, VOTER_C]
        .into_iter()
        .map(occupant)
        .collect()
}

async fn setup_with(snapshot: GuildSnapshot) -> (GuildRoomWorker<Store, Http>, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot);
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    (worker, trace)
}

async fn setup() -> (GuildRoomWorker<Store, Http>, Trace) {
    setup_with(snapshot(&[ROOM], roster())).await
}

fn start(
    worker: &mut GuildRoomWorker<Store, Http>,
    initiator: u64,
    target: u64,
) -> Result<VoteKickUpdate, KickRefusal> {
    worker.kick_start(VOTE, ROOM, initiator, target, 0)
}

#[tokio::test]
async fn strict_majority_passes_and_enforces_on_that_room_only() {
    let (mut worker, trace) = setup().await;
    let started = start(&mut worker, VOTER_A, TARGET).unwrap();
    assert_eq!(started.status, VoteKickStatus::Active);
    // Four voters besides the target: three Yes votes are a strict majority.
    assert_eq!(started.progress.required_total_text(), "3/4");
    for voter in [VOTER_A, VOTER_B] {
        let update = worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
        assert_eq!(update.status, VoteKickStatus::Active);
    }
    assert!(
        !worker.dispatch_one(2).await,
        "nothing queued before the vote passes"
    );
    let passed = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 3).unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
    dispatch(&mut worker, 4).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            format!("deny:{ROOM}:{TARGET}"),
            format!("disconnect:{GUILD}:{TARGET}")
        ]
    );
    // Replay of a settled vote never queues a second enforcement.
    let replay = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 5).unwrap();
    assert_eq!(replay.status, VoteKickStatus::Passed);
    assert!(!worker.dispatch_one(6).await);
}

#[tokio::test]
async fn not_voting_counts_as_no() {
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    worker.kick_cast(VOTE, VOTER_B, VoteBallot::Yes, 2).unwrap();
    worker.kick_cast(VOTE, VOTER_C, VoteBallot::No, 3).unwrap();
    assert!(worker.kick_refresh(4).is_empty());
    assert!(!worker.dispatch_one(5).await);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn owner_original_creator_and_self_cannot_be_targeted() {
    let (mut worker, trace) = setup().await;
    assert_eq!(
        start(&mut worker, VOTER_A, OWNER).unwrap_err(),
        KickRefusal::Vote(VoteKickError::ProtectedTarget)
    );
    assert_eq!(
        start(&mut worker, VOTER_A, VOTER_A).unwrap_err(),
        KickRefusal::Vote(VoteKickError::SelfTarget)
    );
    assert!(trace.lock().unwrap().is_empty());

    // After a handoff the caretaker is the owner and the creator stays
    // protected: neither can be targeted.
    let (mut worker, _) = setup().await;
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = VOTER_A;
    assert_eq!(
        worker
            .kick_start(VOTE, ROOM, VOTER_B, VOTER_A, 0)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::ProtectedTarget)
    );
    assert_eq!(
        worker
            .kick_start(VOTE + 1, ROOM, VOTER_B, OWNER, 0)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::ProtectedTarget)
    );
}

#[tokio::test]
async fn only_occupants_start_and_vote_and_the_target_cannot_vote() {
    let (mut worker, _) = setup().await;
    assert_eq!(
        start(&mut worker, 999, TARGET).unwrap_err(),
        KickRefusal::Vote(VoteKickError::InitiatorNotOccupant)
    );
    assert_eq!(
        start(&mut worker, VOTER_A, 999).unwrap_err(),
        KickRefusal::Vote(VoteKickError::TargetNotOccupant)
    );
    start(&mut worker, VOTER_A, TARGET).unwrap();
    for outsider in [999, TARGET] {
        assert_eq!(
            worker
                .kick_cast(VOTE, outsider, VoteBallot::Yes, 1)
                .unwrap_err(),
            KickRefusal::Vote(VoteKickError::IneligibleVoter)
        );
    }
}

#[tokio::test]
async fn repeat_votes_and_second_votes_for_one_target_are_refused() {
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    assert_eq!(
        worker
            .kick_cast(VOTE, VOTER_A, VoteBallot::No, 2)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::RepeatedVote)
    );
    assert_eq!(
        worker
            .kick_start(VOTE + 1, ROOM, VOTER_B, TARGET, 3)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::ActiveVoteExists)
    );
}

#[tokio::test]
async fn a_button_for_an_unknown_vote_is_refused() {
    let (mut worker, _) = setup().await;
    assert_eq!(
        worker
            .kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 0)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::UnknownVote)
    );
}

#[tokio::test]
async fn untracked_channel_is_not_a_room() {
    let (mut worker, _) = setup().await;
    assert_eq!(
        worker
            .kick_start(VOTE, ROOM + 1, VOTER_A, TARGET, 0)
            .unwrap_err(),
        KickRefusal::NotARoom
    );
}

#[tokio::test]
async fn target_leaving_cancels_the_vote_without_enforcement() {
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    worker.live.voice_update(TARGET, None, Some(false));
    let finished = worker.kick_refresh(2);
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
    assert!(
        worker.kick_refresh(3).is_empty(),
        "a finished vote is forgotten"
    );
    assert!(!worker.dispatch_one(4).await);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn deleted_room_cancels_its_vote() {
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.rooms.remove(&ROOM);
    let finished = worker.kick_refresh(1);
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft)
    );
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn vote_expires_after_two_minutes_without_enforcement() {
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    assert!(worker.kick_refresh(VOTE_KICK_TTL_MS - 1).is_empty());
    let finished = worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].status, VoteKickStatus::Expired);
    let late = worker
        .kick_cast(VOTE, VOTER_B, VoteBallot::Yes, VOTE_KICK_TTL_MS + 1)
        .unwrap();
    assert_eq!(late.status, VoteKickStatus::Expired);
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stale_roster_never_settles_or_starts_a_vote() {
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.live.disconnect();
    assert!(worker.kick_refresh(VOTE_KICK_TTL_MS).is_empty());
    assert_eq!(
        worker
            .kick_cast(VOTE, VOTER_B, VoteBallot::Yes, 1)
            .unwrap_err(),
        KickRefusal::Unavailable
    );
    assert_eq!(
        worker
            .kick_start(VOTE + 1, ROOM, VOTER_B, VOTER_C, 1)
            .unwrap_err(),
        KickRefusal::Unavailable
    );
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bots_are_not_occupants_for_voting() {
    let mut members = roster();
    members.push(VoiceMember {
        member_id: 777,
        channel_id: ROOM,
        bot: Some(true),
    });
    let (mut worker, _) = setup_with(snapshot(&[ROOM], members)).await;
    let started = start(&mut worker, VOTER_A, TARGET).unwrap();
    assert_eq!(started.progress.total, 4);
    assert_eq!(
        worker.kick_cast(VOTE, 777, VoteBallot::Yes, 1).unwrap_err(),
        KickRefusal::Vote(VoteKickError::IneligibleVoter)
    );
}

/// Start a vote on `TARGET` and pass it with three Yes ballots.
fn pass_vote(worker: &mut GuildRoomWorker<Store, Http>) {
    start(worker, VOTER_A, TARGET).unwrap();
    for voter in [VOTER_A, VOTER_B] {
        worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
    }
    let passed = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 2).unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
}

#[tokio::test]
async fn target_moved_to_another_channel_is_denied_but_not_disconnected() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    // Between the pass and the dispatch the target joins a channel nobody voted in.
    worker
        .live
        .voice_update(TARGET, Some(ROOM + 1), Some(false));
    dispatch(&mut worker, 3).await;
    assert_eq!(*trace.lock().unwrap(), [format!("deny:{ROOM}:{TARGET}")]);
    assert!(
        worker.failures().is_empty(),
        "a skipped disconnect is not a failure"
    );
}

#[tokio::test]
async fn target_who_left_voice_is_denied_but_not_disconnected() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    worker.live.voice_update(TARGET, None, Some(false));
    dispatch(&mut worker, 3).await;
    assert_eq!(*trace.lock().unwrap(), [format!("deny:{ROOM}:{TARGET}")]);
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn target_who_became_owner_after_the_vote_passed_is_not_touched() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = TARGET;
    dispatch(&mut worker, 3).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn target_who_is_the_original_creator_after_the_vote_passed_is_not_touched() {
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    worker.rooms.get_mut(&ROOM).unwrap().original_creator_id = TARGET;
    dispatch(&mut worker, 3).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.failures().is_empty());
}

#[tokio::test]
async fn missing_manage_roles_records_a_failure_and_writes_nothing() {
    let mut snap = snapshot(&[ROOM], roster());
    snap.bot.roles = vec![role(permissions().difference(Permissions::MANAGE_ROLES))];
    let (mut worker, trace) = setup_with(snap).await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    for voter in [VOTER_A, VOTER_B, VOTER_C] {
        worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
    }
    dispatch(&mut worker, 2).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.failures().iter().any(|failure| matches!(
        failure,
        LifecycleFailure::Discord {
            channel_id: ROOM,
            error: RoomHttpError::AccessDenied
        }
    )));
}

#[tokio::test]
async fn actor_plumbing_refuses_unknown_rooms_and_votes() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime
        .kick_start(GUILD, VOTE, ROOM, VOTER_A, TARGET)
        .await
        .is_none());
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[], vec![])));
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.kick_start(GUILD, VOTE, ROOM, VOTER_A, TARGET),
    )
    .await
    .expect("start reply")
    .expect("live actor");
    assert_eq!(started.unwrap_err(), KickRefusal::NotARoom);
    let ballot = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.kick_ballot(GUILD, VOTE, VOTER_A, VoteBallot::Yes),
    )
    .await
    .expect("ballot reply")
    .expect("live actor");
    assert_eq!(
        ballot.unwrap_err(),
        KickRefusal::Vote(VoteKickError::UnknownVote)
    );
}
