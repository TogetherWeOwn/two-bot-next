#![no_main]

//! Arbitrary start/vote/refresh sequences over [`VoteKickCore`].
//!
//! The vote-kick decision core is a pure, time-ordered state machine: the
//! caller supplies room facts and a clock, and the core emits a room-scoped
//! kick decision at most once. Twelve input bytes drive one transition, so
//! arbitrary fuzzer bytes become arbitrary sequences of starts, ballots,
//! refreshes, roster changes and clock jumps. Small synthetic ID pools force
//! vote-ID reuse, shared guild/target pairs and outsider actors, keeping the
//! replay, active-vote and boundary rejections reachable instead of starving
//! behind fresh IDs. No Discord client, database, secret or network is used.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use libfuzzer_sys::fuzz_target;
use two_bot_core::voice_vote_kick::{
    VoteBallot, VoteClock, VoteKickCore, VoteKickError, VoteKickRef, VoteKickStatus,
    VoteKickUpdate, VoteProgress, VoteRoomFacts,
};

struct FuzzClock(Cell<u64>);

impl VoteClock for FuzzClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// Occupant seed pool. 99 is never a seeded occupant; it exercises outsider
/// initiators and voters through [`ACTORS`].
const MEMBERS: [u64; 5] = [2, 3, 4, 5, 9];
const ACTORS: [u64; 6] = [2, 3, 4, 5, 9, 99];
const VOTE_IDS: [u64; 3] = [100, 101, 102];
const GUILDS: [u64; 2] = [1, 7];
const ROOMS: [u64; 2] = [10, 11];

/// Twelve bytes drive one transition; a short tail is dropped.
const STRIDE: usize = 12;

fn pick(pool: &[u64], byte: u8) -> u64 {
    pool[usize::from(byte) % pool.len()]
}

fn occupants(byte: u8) -> Vec<u64> {
    let mut ids: Vec<u64> = MEMBERS
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| byte & (1 << index) != 0)
        .map(|(_, id)| id)
        .collect();
    // Occasionally duplicate the roster, matching the deduplicated-roster unit test.
    if byte & 0x20 != 0 {
        if let Some(first) = ids.first().copied() {
            ids.push(first);
        }
    }
    ids
}

fn apply_clock_jump(now: u64, mode: u8) -> u64 {
    match mode % 6 {
        // Small forward step.
        0 => now.wrapping_add(u64::from(mode) % 32),
        // Stride past the 2-minute TTL to force expiry.
        1 => now.wrapping_add(u64::from(mode) * 1_000),
        // Step back so the before-start InvalidTime path stays reachable.
        2 => now.saturating_sub(u64::from(mode) % 32),
        // Before the 1_000 start baseline.
        3 => 999,
        // Reset to the baseline.
        4 => 1_000,
        // Deadline-overflow edge for start's checked_add.
        _ => u64::MAX - u64::from(mode),
    }
}

fn check_progress(progress: VoteProgress) {
    assert_eq!(progress.required, progress.total / 2 + 1);
    assert!(progress.yes <= progress.total);
}

fn drive(data: &[u8]) {
    let mut core = VoteKickCore::new();
    let clock = FuzzClock(Cell::new(1_000));
    let mut started: BTreeSet<u64> = BTreeSet::new();
    let mut emitted: BTreeSet<u64> = BTreeSet::new();
    let mut terminal: BTreeMap<u64, VoteKickStatus> = BTreeMap::new();

    let mut check_update = |id: u64, update: VoteKickUpdate| {
        check_progress(update.progress);
        if let Some(kick) = update.kick {
            // The kick emits once, on the first Active -> Passed transition,
            // scoped to the vote's own guild, room and target.
            assert_eq!(update.status, VoteKickStatus::Passed);
            assert_eq!(kick.guild_id, update.vote.guild_id);
            assert_eq!(kick.room_id, update.vote.room_id);
            assert_eq!(kick.target_id, update.vote.target_id);
            assert!(emitted.insert(id), "kick emitted twice");
            terminal.insert(id, VoteKickStatus::Passed);
        } else if let Some(previous) = terminal.get(&id) {
            // Terminal votes stay terminal: replay reads the status, never a
            // new kick, and a rejoining target cannot revive a cancelled vote.
            assert_eq!(update.status, *previous);
        } else if update.status != VoteKickStatus::Active {
            terminal.insert(id, update.status);
        }
    };

    for step in data.chunks_exact(STRIDE) {
        clock.0.set(apply_clock_jump(clock.0.get(), step[1]));
        let id = pick(&VOTE_IDS, step[2]);
        let reference = VoteKickRef {
            id,
            guild_id: pick(&GUILDS, step[3]),
            room_id: pick(&ROOMS, step[4]),
            target_id: pick(&ACTORS, step[5]),
        };
        let room = occupants(step[10]);
        let facts = VoteRoomFacts {
            guild_id: pick(&GUILDS, step[6]),
            room_id: pick(&ROOMS, step[7]),
            owner_id: pick(&ACTORS, step[8]),
            original_creator_id: pick(&ACTORS, step[9]),
            occupants: &room,
        };
        let actor = pick(&ACTORS, step[11]);
        let ballot = if step[10] & 0x40 != 0 {
            VoteBallot::Yes
        } else {
            VoteBallot::No
        };
        match step[0] % 3 {
            0 => match core.start(id, facts, actor, reference.target_id, &clock) {
                Ok(update) => {
                    // Starting never casts a ballot or emits a kick; the vote
                    // opens Active and binds the facts' own scope.
                    assert_eq!(update.status, VoteKickStatus::Active);
                    assert_eq!(update.kick, None);
                    assert_eq!(update.vote.id, id);
                    assert_eq!(update.vote.guild_id, facts.guild_id);
                    assert_eq!(update.vote.room_id, facts.room_id);
                    assert_eq!(update.vote.target_id, reference.target_id);
                    check_progress(update.progress);
                    started.insert(id);
                }
                Err(VoteKickError::ReusedVoteId) => {
                    // Replay refused: the ID must come from an earlier start.
                    assert!(started.contains(&id));
                }
                Err(_) => {}
            },
            1 => match core.cast(reference, facts, actor, ballot, &clock) {
                Ok(update) => check_update(id, update),
                Err(_) => {}
            },
            _ => match core.refresh(reference, facts, &clock) {
                Ok(update) => check_update(id, update),
                Err(_) => {}
            },
        }
    }
}

fuzz_target!(|data: &[u8]| {
    drive(data);
});
