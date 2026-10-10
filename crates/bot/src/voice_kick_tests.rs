use super::*;
use two_bot_core::voice_vote_kick::{
    VoteBallot, VoteCancellation, VoteKickError, VOTE_KICK_COOLDOWN_MS, VOTE_KICK_TTL_MS,
};

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

const KICK_ROLE: u64 = 901;
const ADMIN_ROLE: u64 = 902;

/// Give `target` a guild role carrying `permissions` in this worker's guild,
/// so VK-01 guild-scoped resolution observes it on the next vote transition.
fn grant_guild_role(
    worker: &GuildRoomWorker<Store, Http>,
    target: u64,
    role: u64,
    permissions: Permissions,
) {
    {
        let live = worker.live.read_state();
        if !live
            .bot
            .as_ref()
            .is_some_and(|bot| bot.roles.iter().any(|r| r.id.get() == role))
        {
            drop(live);
            worker
                .live
                .write_state()
                .bot
                .as_mut()
                .expect("bot snapshot")
                .roles
                .push(role_with(role, permissions));
        }
    }
    worker.live.set_member_roles(target, Some(vec![role]));
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
async fn timer_prune_reaps_evicted_vote_refs_past_the_cooldown_horizon() {
    let (mut worker, _) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    worker.kick_cast(VOTE, VOTER_B, VoteBallot::Yes, 2).unwrap();
    let passed = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 3).unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
    // Through the horizon inclusive a stale button still replays terminal.
    let horizon_end = 3 + VOTE_KICK_COOLDOWN_MS;
    let replay = worker
        .kick_cast(VOTE, VOTER_C, VoteBallot::Yes, horizon_end)
        .unwrap();
    assert_eq!(replay.status, VoteKickStatus::Passed);
    // Strictly past the horizon the timer reaps the core vote and this
    // worker's ref maps: the stale button is unknown and the target is
    // votable again with a fresh interaction ID.
    let after = horizon_end + 1;
    assert!(worker.kick_refresh(after).is_empty());
    assert!(worker.vote_refs.get(&VOTE).is_none());
    assert_eq!(
        worker
            .kick_cast(VOTE, VOTER_A, VoteBallot::Yes, after)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::UnknownVote)
    );
    worker
        .kick_start(VOTE + 1, ROOM, VOTER_A, TARGET, after)
        .unwrap();
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
async fn kick_members_and_administrator_targets_are_refused_with_no_effect() {
    // VK-01 start path: each privileged class is denied separately in the
    // interaction's guild, creating no vote, ballot, or enforcement effect.
    for (role, permissions) in [
        (KICK_ROLE, Permissions::KICK_MEMBERS),
        (ADMIN_ROLE, Permissions::ADMINISTRATOR),
    ] {
        let (mut worker, trace) = setup().await;
        grant_guild_role(&worker, TARGET, role, permissions);
        assert_eq!(
            start(&mut worker, VOTER_A, TARGET).unwrap_err(),
            KickRefusal::Vote(VoteKickError::PrivilegedTarget),
            "role {role}"
        );
        // Refusal created no vote: a ballot is unknown and nothing is queued.
        assert_eq!(
            worker
                .kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1)
                .unwrap_err(),
            KickRefusal::Vote(VoteKickError::UnknownVote)
        );
        assert!(worker.kick_refresh(2).is_empty());
        assert!(!worker.dispatch_one(3).await);
        assert!(trace.lock().unwrap().is_empty());
        // The refusal is audited with its own code.
        assert!(worker.flush_kick_audit(4).await);
        let rows = worker.store.kick_audit.lock().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event, KickAuditEvent::VoteRefused);
        assert_eq!(rows[0].outcome, "privileged_target");
    }
}

#[tokio::test]
async fn unavailable_target_authority_fails_closed_with_no_effect() {
    // VK-01 fail-closed: losing the guild-authority lookup refuses the start
    // with no vote, ballot, or Discord write.
    let (mut worker, trace) = setup().await;
    worker.live.set_member_roles(TARGET, None);
    assert_eq!(
        start(&mut worker, VOTER_A, TARGET).unwrap_err(),
        KickRefusal::Vote(VoteKickError::AuthorityUnavailable)
    );
    assert_eq!(
        worker
            .kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1)
            .unwrap_err(),
        KickRefusal::Vote(VoteKickError::UnknownVote)
    );
    assert!(worker.kick_refresh(2).is_empty());
    assert!(!worker.dispatch_one(3).await);
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.flush_kick_audit(4).await);
    let rows = worker.store.kick_audit.lock().unwrap().clone();
    assert_eq!(rows[0].outcome, "authority_unavailable");
}

#[tokio::test]
async fn promoted_target_mid_vote_cancels_and_enforces_nothing() {
    // VK-01 recheck: a Kick Members grant after the start cancels the vote;
    // the would-be passing ballot produces no kick/disconnect effect.
    let (mut worker, trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    worker.kick_cast(VOTE, VOTER_A, VoteBallot::Yes, 1).unwrap();
    grant_guild_role(&worker, TARGET, KICK_ROLE, Permissions::KICK_MEMBERS);
    let update = worker.kick_cast(VOTE, VOTER_B, VoteBallot::Yes, 2).unwrap();
    assert_eq!(
        update.status,
        VoteKickStatus::Cancelled(VoteCancellation::TargetProtected)
    );
    assert_eq!(update.kick, None);
    assert!(!worker.dispatch_one(3).await);
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn promoted_target_after_pass_is_not_touched_on_dispatch() {
    // A promotion landing between the pass and the Discord writes must still
    // produce zero writes: the enforcement recheck fails closed.
    let (mut worker, trace) = setup().await;
    pass_vote(&mut worker);
    grant_guild_role(&worker, TARGET, ADMIN_ROLE, Permissions::ADMINISTRATOR);
    dispatch(&mut worker, 3).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.failures().is_empty());
    assert!(worker.flush_kick_audit(4).await);
    let enforcement: Vec<_> = worker
        .store
        .kick_audit
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row.event == KickAuditEvent::Enforcement)
        .cloned()
        .collect();
    assert_eq!(enforcement.len(), 1);
    assert_eq!(enforcement[0].outcome, "skipped_target_protected");
}

#[tokio::test]
async fn ordinary_target_remains_votable_end_to_end() {
    // An occupant with no privilege, who is not owner/creator/self, still
    // passes and enforces on that room only.
    let (mut worker, trace) = setup().await;
    let started = start(&mut worker, VOTER_A, TARGET).unwrap();
    assert_eq!(started.status, VoteKickStatus::Active);
    for voter in [VOTER_A, VOTER_B] {
        worker.kick_cast(VOTE, voter, VoteBallot::Yes, 1).unwrap();
    }
    let passed = worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 2).unwrap();
    assert_eq!(passed.status, VoteKickStatus::Passed);
    dispatch(&mut worker, 3).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            format!("deny:{ROOM}:{TARGET}"),
            format!("disconnect:{GUILD}:{TARGET}")
        ]
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

#[tokio::test]
async fn kick_room_of_resolves_tracked_rooms_only() {
    let (worker, _trace) = setup().await;
    assert_eq!(worker.kick_room_of(TARGET), Some(ROOM));
    assert_eq!(worker.kick_room_of(VOTER_A), Some(ROOM));
    // Unknown member: no room, so the router keeps the interaction.
    assert_eq!(worker.kick_room_of(999_999), None);
}

#[tokio::test]
async fn kick_room_of_ignores_untracked_channels() {
    const LOBBY: u64 = 501;
    const BYSTANDER: u64 = 305;
    let mut members = roster();
    members.push(VoiceMember {
        member_id: BYSTANDER,
        channel_id: LOBBY,
        bot: Some(false),
    });
    // LOBBY is a live voice channel but not a tracked temporary room.
    let (worker, _trace) = setup_with(snapshot(&[ROOM, LOBBY], members)).await;
    assert_eq!(worker.kick_room_of(BYSTANDER), None);
    assert_eq!(worker.kick_room_of(TARGET), Some(ROOM));
}

#[tokio::test]
async fn kick_room_of_is_none_while_evidence_is_stale() {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    // No snapshot published: evidence is not authoritative.
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    assert_eq!(worker.kick_room_of(TARGET), None);
}

#[tokio::test]
async fn actor_kick_room_of_answers_through_the_inbox() {
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, snapshot(&[ROOM], roster())));
    // The fake store tracks no rooms, so even a live-occupant target is
    // unclaimed; the inbox round-trip answers instead of hanging.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), runtime.kick_room_of(GUILD, TARGET),)
            .await
            .expect("claim reply"),
        None
    );
}

// --- V4 audit trail --------------------------------------------------------

/// Everything appended so far, as (event, outcome) in append order.
fn audit_trail(worker: &GuildRoomWorker<Store, Http>) -> Vec<(KickAuditEvent, &'static str)> {
    worker
        .store
        .kick_audit
        .lock()
        .unwrap()
        .iter()
        .map(|row| (row.event, row.outcome))
        .collect()
}

#[tokio::test]
async fn a_passed_vote_audits_start_result_and_enforcement_with_every_id() {
    let (mut worker, _trace) = setup().await;
    pass_vote(&mut worker);
    dispatch(&mut worker, 3).await;
    assert!(worker.flush_kick_audit(4).await);
    assert_eq!(
        audit_trail(&worker),
        [
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteResult, "passed"),
            (
                KickAuditEvent::Enforcement,
                "connect_denied_and_disconnected"
            ),
        ]
    );
    let rows = worker.store.kick_audit.lock().unwrap().clone();
    for row in &rows {
        assert_eq!(
            (
                row.guild_id,
                row.room_id,
                row.vote_id,
                row.initiator_id,
                row.target_id
            ),
            (GUILD, ROOM, VOTE, VOTER_A, TARGET),
            "{row:?}"
        );
        assert!(
            two_bot_core::parse_iso_millis(&row.occurred_at).is_some(),
            "{row:?}"
        );
    }
    let progress: Vec<_> = rows
        .iter()
        .map(|row| row.progress.map(|p| (p.yes, p.required, p.total)))
        .collect();
    assert_eq!(progress, [Some((0, 3, 4)), Some((3, 3, 4)), None]);
    // Replaying a settled vote adds nothing.
    worker.kick_cast(VOTE, VOTER_C, VoteBallot::Yes, 5).unwrap();
    assert!(!worker.flush_kick_audit(6).await);
    assert_eq!(rows.len(), worker.store.kick_audit.lock().unwrap().len());
}

#[tokio::test]
async fn refused_starts_are_audited_with_their_refusal_code() {
    let (mut worker, _trace) = setup().await;
    assert!(start(&mut worker, VOTER_A, OWNER).is_err());
    assert!(worker
        .kick_start(VOTE + 1, ROOM, VOTER_A, VOTER_A, 0)
        .is_err());
    assert!(worker
        .kick_start(VOTE + 2, ROOM + 1, VOTER_A, TARGET, 0)
        .is_err());
    assert!(worker.kick_start(VOTE + 3, ROOM, 999, TARGET, 0).is_err());
    worker.live.disconnect();
    assert!(worker
        .kick_start(VOTE + 4, ROOM, VOTER_A, TARGET, 0)
        .is_err());
    assert!(worker.flush_kick_audit(1).await);
    assert_eq!(
        audit_trail(&worker),
        [
            (KickAuditEvent::VoteRefused, "protected_target"),
            (KickAuditEvent::VoteRefused, "self_target"),
            (KickAuditEvent::VoteRefused, "not_a_room"),
            (KickAuditEvent::VoteRefused, "initiator_not_occupant"),
            (KickAuditEvent::VoteRefused, "evidence_unavailable"),
        ]
    );
    let rows = worker.store.kick_audit.lock().unwrap().clone();
    // The refused row names the attempt, never a vote that did not start.
    assert_eq!(
        rows.iter().map(|r| r.vote_id).collect::<Vec<_>>(),
        [VOTE, VOTE + 1, VOTE + 2, VOTE + 3, VOTE + 4]
    );
    assert_eq!(rows[0].initiator_id, VOTER_A);
    assert_eq!(rows[0].target_id, OWNER);
    assert!(rows.iter().all(|row| row.progress.is_none()));
}

#[tokio::test]
async fn a_second_vote_for_one_target_is_audited_as_refused_next_to_the_first() {
    let (mut worker, _trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    assert!(worker
        .kick_start(VOTE + 1, ROOM, VOTER_B, TARGET, 1)
        .is_err());
    assert!(worker.flush_kick_audit(2).await);
    assert_eq!(
        audit_trail(&worker),
        [
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteRefused, "active_vote_exists"),
        ]
    );
}

#[tokio::test]
async fn expiry_and_cancellation_audit_one_result_each_and_no_enforcement() {
    let (mut worker, _trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    assert_eq!(worker.kick_refresh(VOTE_KICK_TTL_MS).len(), 1);
    // A late ballot and another refresh restate the terminal status only.
    worker
        .kick_cast(VOTE, VOTER_B, VoteBallot::Yes, VOTE_KICK_TTL_MS + 1)
        .unwrap();
    assert!(worker.kick_refresh(VOTE_KICK_TTL_MS + 2).is_empty());
    worker
        .kick_start(VOTE + 1, ROOM, VOTER_B, VOTER_C, VOTE_KICK_TTL_MS + 3)
        .unwrap();
    worker.live.voice_update(VOTER_C, None, Some(false));
    assert_eq!(worker.kick_refresh(VOTE_KICK_TTL_MS + 4).len(), 1);
    assert!(worker.flush_kick_audit(VOTE_KICK_TTL_MS + 5).await);
    assert_eq!(
        audit_trail(&worker),
        [
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteResult, "expired"),
            (KickAuditEvent::VoteStarted, "started"),
            (KickAuditEvent::VoteResult, "cancelled_target_left"),
        ]
    );
    let rows = worker.store.kick_audit.lock().unwrap().clone();
    assert_eq!(
        (rows[3].vote_id, rows[3].initiator_id, rows[3].target_id),
        (VOTE + 1, VOTER_B, VOTER_C)
    );
}

/// Pass a vote, apply `change` to the worker, dispatch the enforcement, flush
/// and return the enforcement outcome code.
async fn enforcement_outcome(
    mut worker: GuildRoomWorker<Store, Http>,
    change: impl FnOnce(&mut GuildRoomWorker<Store, Http>),
) -> &'static str {
    pass_vote(&mut worker);
    change(&mut worker);
    dispatch(&mut worker, 3).await;
    assert!(worker.flush_kick_audit(4).await);
    let rows = worker.store.kick_audit.lock().unwrap().clone();
    let enforcement: Vec<_> = rows
        .iter()
        .filter(|row| row.event == KickAuditEvent::Enforcement)
        .collect();
    assert_eq!(enforcement.len(), 1, "exactly one enforcement row");
    assert_eq!(enforcement[0].vote_id, VOTE);
    enforcement[0].outcome
}

#[tokio::test]
async fn enforcement_rows_say_what_discord_was_asked_never_a_kick() {
    let (worker, _) = setup().await;
    assert_eq!(
        enforcement_outcome(worker, |_| {}).await,
        "connect_denied_and_disconnected"
    );
    let (worker, _) = setup().await;
    assert_eq!(
        enforcement_outcome(worker, |w| {
            w.live.voice_update(TARGET, None, Some(false));
        })
        .await,
        "connect_denied_target_absent"
    );
    let (worker, _) = setup().await;
    assert_eq!(
        enforcement_outcome(worker, |w| w.rooms.get_mut(&ROOM).unwrap().owner_id =
            TARGET)
        .await,
        "skipped_target_protected"
    );
    let (worker, _) = setup().await;
    assert_eq!(
        enforcement_outcome(worker, |w| {
            w.rooms.remove(&ROOM);
        })
        .await,
        "skipped_room_gone"
    );
}

#[tokio::test]
async fn enforcement_refused_for_permissions_or_by_discord_is_audited_once() {
    let mut snap = snapshot(&[ROOM], roster());
    snap.bot.roles = vec![role(permissions().difference(Permissions::MANAGE_ROLES))];
    let (worker, _) = setup_with(snap).await;
    assert_eq!(
        enforcement_outcome(worker, |_| {}).await,
        "permission_missing"
    );
    let (worker, _) = setup().await;
    assert_eq!(
        enforcement_outcome(worker, |w| {
            w.http
                .kick_errors
                .lock()
                .unwrap()
                .push_back(RoomHttpError::Rejected {
                    status: 400,
                    code: 50_035,
                });
        })
        .await,
        "discord_error"
    );
}

#[tokio::test]
async fn a_retryable_enforcement_failure_is_audited_only_when_it_gives_up() {
    let (mut worker, _trace) = setup().await;
    pass_vote(&mut worker);
    {
        let mut errors = worker.http.kick_errors.lock().unwrap();
        for _ in 0..QUEUE_MAX_ATTEMPTS {
            errors.push_back(RoomHttpError::UnknownOutcome);
        }
    }
    let mut now = 3;
    for attempt in 1..=QUEUE_MAX_ATTEMPTS {
        now += 10_000_000;
        assert!(worker.dispatch_one(now).await, "attempt {attempt}");
        worker.flush_kick_audit(now).await;
        let enforcement = audit_trail(&worker)
            .iter()
            .filter(|(event, _)| *event == KickAuditEvent::Enforcement)
            .count();
        assert_eq!(
            enforcement,
            usize::from(attempt == QUEUE_MAX_ATTEMPTS),
            "attempt {attempt}"
        );
    }
    assert_eq!(audit_trail(&worker).last().unwrap().1, "gave_up");
}

#[tokio::test]
async fn a_failed_flush_keeps_the_rows_backs_off_and_never_halts_the_worker() {
    let (mut worker, _trace) = setup().await;
    start(&mut worker, VOTER_A, TARGET).unwrap();
    for error in [StoreError::Unavailable, StoreError::CredentialRefused] {
        worker
            .store
            .kick_audit_errors
            .lock()
            .unwrap()
            .push_back(error);
    }
    assert!(!worker.flush_kick_audit(0).await);
    assert!(worker.store.kick_audit.lock().unwrap().is_empty());
    assert!(worker.failures().iter().any(|failure| matches!(
        failure,
        LifecycleFailure::Persistence {
            channel_id: None,
            error: StoreError::Unavailable
        }
    )));
    // Inside the backoff nothing is attempted, so the second scripted error
    // is still queued.
    assert!(!worker.flush_kick_audit(KICK_AUDIT_RETRY_MS - 1).await);
    assert_eq!(worker.store.kick_audit_errors.lock().unwrap().len(), 1);
    // A refused credential (a grant not applied yet) must not halt rooms.
    assert!(!worker.flush_kick_audit(KICK_AUDIT_RETRY_MS).await);
    assert!(!worker.halted());
    assert!(worker.flush_kick_audit(KICK_AUDIT_RETRY_MS * 2).await);
    assert_eq!(
        audit_trail(&worker),
        [(KickAuditEvent::VoteStarted, "started")]
    );
    assert!(!worker.flush_kick_audit(KICK_AUDIT_RETRY_MS * 3).await);
}

/// A freshly started vote, for render-only tests: no worker needed.
fn started_update() -> VoteKickUpdate {
    VoteKickUpdate {
        vote: VoteKickRef {
            id: VOTE,
            guild_id: GUILD,
            room_id: ROOM,
            target_id: TARGET,
        },
        status: VoteKickStatus::Active,
        progress: VoteProgress {
            yes: 0,
            required: 3,
            total: 4,
        },
        kick: None,
    }
}

/// The final Discord payload for a vote start with `reason`, split into the
/// public content and the full response for mention/flag assertions.
fn start_payload(reason: Option<&str>) -> (String, InteractionResponse) {
    let response = vote_message(&started_update(), VOTER_A, TARGET, reason);
    let content = response
        .data
        .as_ref()
        .and_then(|data| data.content.clone())
        .unwrap_or_default();
    (content, response)
}

/// The rendered `Reason:` line of a vote-start payload.
fn reason_line(content: &str) -> &str {
    content
        .lines()
        .find(|line| line.starts_with("Reason: "))
        .expect("a Reason line")
}

/// Gate VK-04 hostile matrix, asserted on the final Discord payload: no
/// hostile reason may produce a ping, clickable link, embed or formatted bot
/// endorsement in the wire text.
#[test]
fn hostile_reasons_render_as_mention_safe_plain_text() {
    for hostile in [
        "@everyone get in here",
        "@here vote yes",
        "@\u{200b}everyone split obfuscation",
        "@\u{200c}here split obfuscation",
        "<@&7654321> role pill",
        "<@987654321> user pill",
        "<#123456789> channel pill",
        "<:custom:123456789> emoji pill",
        "<a:dance:123456789> animated emoji pill",
        "see https://evil.example/phish for proof",
        "see http://evil.example/phish for proof",
        "see HTTPS://evil.example/phish for proof",
        "[click here](https://evil.example/phish)",
        "www.evil.example/phish",
        "WWW.EVIL.EXAMPLE/PHISH",
        "Www.evil.example/phish",
        "join discord.gg/abc123 for backup",
        "join DISCORD.GG/ABC123 for backup",
        "visit evil.com/phish for proof",
        "visit EVIL.COM/PHISH for proof",
        "**BAN THEM** __now__ ~~please~~ `code` ||spoiler||",
        "# heading\n> quote\n```fence```\n- list\nmultiline",
    ] {
        let (content, response) = start_payload(Some(hostile));
        let line = reason_line(&content);
        assert!(!line.contains("@everyone"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("@here"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("<@"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("<#"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("<:"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("<a:"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("://"), "{hostile:?} -> {line:?}");
        assert!(!line.contains("www."), "{hostile:?} -> {line:?}");
        // Uppercase variants must not survive either: the `www.` scan and
        // the bare-domain dot split are case-insensitive.
        let folded = line.to_lowercase();
        assert!(!folded.contains("www."), "{hostile:?} -> {line:?}");
        assert!(!folded.contains("discord.gg"), "{hostile:?} -> {line:?}");
        assert!(!folded.contains(".gg/"), "{hostile:?} -> {line:?}");
        assert!(!folded.contains(".com/"), "{hostile:?} -> {line:?}");
        for markup in ["**", "__", "~~", "||", "[click here]("] {
            assert!(!line.contains(markup), "{hostile:?} -> {line:?}");
        }
        // The reason is one folded line: no block markup can start a line.
        assert_eq!(content.lines().count(), 3, "{hostile:?} -> {content:?}");
        // Payload fences hold for every hostile input: only the target may be
        // pinged, embeds stay suppressed, and no embed object is attached.
        let data = response.data.as_ref().expect("response data");
        assert_eq!(
            data.allowed_mentions
                .as_ref()
                .map(|mentions| mentions.users.clone())
                .unwrap_or_default(),
            [Id::<UserMarker>::new(TARGET)]
        );
        assert!(data
            .allowed_mentions
            .as_ref()
            .is_some_and(|mentions| mentions.parse.is_empty() && mentions.roles.is_empty()));
        assert!(data
            .flags
            .is_some_and(|flags| flags.contains(MessageFlags::SUPPRESS_EMBEDS)));
        assert!(data.embeds.as_ref().is_none_or(Vec::is_empty));
    }
}

#[test]
fn overlong_reasons_are_cut_to_the_documented_bound_after_escaping() {
    for hostile in ["x".repeat(600), format!("**{}**", "x".repeat(600))] {
        let (content, _) = start_payload(Some(&hostile));
        let rendered = reason_line(&content)
            .strip_prefix("Reason: ")
            .expect("reason text");
        assert_eq!(
            rendered.chars().count(),
            VOTE_KICK_PUBLIC_REASON_LIMIT,
            "overlong input must fill exactly the bound"
        );
    }
    let long = "y".repeat(VOTE_KICK_PUBLIC_REASON_LIMIT + 1);
    let options = vec![CommandDataOption {
        name: "reason".to_owned(),
        value: CommandOptionValue::String(long),
    }];
    assert_eq!(
        parse_kick_reason(&options).map(|reason| reason.chars().count()),
        Some(VOTE_KICK_PUBLIC_REASON_LIMIT),
        "parse caps to the same documented bound"
    );
}

#[test]
fn ordinary_reasons_render_intact_and_absent_reason_renders_no_line() {
    let plain = "Playing loud music after quiet hours";
    let (content, _) = start_payload(Some(plain));
    assert!(
        content.contains(&format!("Reason: {plain}")),
        "plain text must pass through byte-identical: {content:?}"
    );
    let (content, _) = start_payload(None);
    assert!(
        !content.lines().any(|line| line.starts_with("Reason:")),
        "no reason means no Reason line: {content:?}"
    );
}

/// Gate VK-04 refusal half: every vote-kick refusal is a fixed acknowledgement
/// that never interpolates initiator text, so raw input cannot leak through an
/// error path.
#[test]
fn kick_refusals_never_echo_initiator_text() {
    let refusals = [
        KickRefusal::Unavailable,
        KickRefusal::NotARoom,
        KickRefusal::Vote(VoteKickError::InitiatorNotOccupant),
        KickRefusal::Vote(VoteKickError::TargetNotOccupant),
        KickRefusal::Vote(VoteKickError::SelfTarget),
        KickRefusal::Vote(VoteKickError::ProtectedTarget),
        KickRefusal::Vote(VoteKickError::ActiveVoteExists),
        KickRefusal::Vote(VoteKickError::ReusedVoteId),
        KickRefusal::Vote(VoteKickError::UnknownVote),
        KickRefusal::Vote(VoteKickError::WrongVoteBoundary),
        KickRefusal::Vote(VoteKickError::IneligibleVoter),
        KickRefusal::Vote(VoteKickError::RepeatedVote),
        KickRefusal::Vote(VoteKickError::InvalidTime),
    ];
    assert_eq!(refusals.len(), 13, "every refusal variant is covered");
    for refusal in refusals {
        let text = kick_refusal_text(&refusal);
        for probe in ["@everyone", "https://", "<@", "**"] {
            assert!(
                !text.contains(probe),
                "{refusal:?} must not echo initiator text: {text:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_audit_buffer_is_bounded_and_drops_the_oldest_row() {
    let (mut worker, _trace) = setup().await;
    for vote in 0..=KICK_AUDIT_BUFFER_MAX as u64 {
        worker
            .kick_start(VOTE + vote, ROOM, VOTER_A, OWNER, 0)
            .unwrap_err();
    }
    assert_eq!(worker.kick_audit.len(), KICK_AUDIT_BUFFER_MAX);
    assert_eq!(
        worker.kick_audit.front().map(|row| row.vote_id),
        Some(VOTE + 1),
        "the oldest row went first"
    );
    assert!(worker.failures().iter().any(|failure| matches!(
        failure,
        LifecycleFailure::Persistence {
            channel_id: None,
            ..
        }
    )));
    // Flushing drains in batches.
    let mut flushes = 0;
    while worker.flush_kick_audit(1).await {
        flushes += 1;
    }
    assert_eq!(flushes, KICK_AUDIT_BUFFER_MAX.div_ceil(KICK_AUDIT_BATCH));
    assert_eq!(
        worker.store.kick_audit.lock().unwrap().len(),
        KICK_AUDIT_BUFFER_MAX
    );
}
