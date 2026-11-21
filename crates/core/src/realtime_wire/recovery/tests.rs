use std::{convert::Infallible, future::pending};

use serde_json::{from_slice, json, Value};

use super::*;
use crate::realtime_wire::{
    tests::{id, now},
    Guild, Member, Permissions, Role,
};

fn topics(guild_id: Id) -> Topics {
    let owner_id = id(300);
    Topics {
        guild: Guild {
            owner_id,
            afk_channel_id: None,
        },
        channels: vec![],
        roles: vec![Role {
            id: guild_id,
            permissions: Permissions(0),
        }],
        members: vec![Member {
            user_id: owner_id,
            role_ids: vec![],
            nick: None,
            bot: false,
            pending: false,
        }],
        voice: vec![],
        events: vec![],
    }
}

async fn unexpected_write(_: Id, _: u64, _: u64) -> Result<(), Infallible> {
    panic!("rejected baseline must not write the cursor")
}

#[tokio::test]
async fn publishes_a_quiet_guild_marker_and_matching_snapshot_at_the_reserved_watermark() {
    let guild_id = id(100);
    let mut recovery = Recovery::new(guild_id);
    assert!(recovery.baseline().is_none());
    let generation = recovery.begin();
    assert!(recovery.baseline().is_none());
    let mut stored_seq = 41;
    let marker = recovery
        .publish(
            &generation,
            41,
            now(),
            topics(guild_id),
            async |guild, expected, next| {
                assert!(guild == guild_id);
                assert_eq!((expected, next), (stored_seq, 43));
                stored_seq = next;
                Ok::<_, Infallible>(())
            },
        )
        .await
        .unwrap();
    let baseline = recovery.baseline().unwrap();
    let snapshot: Value = from_slice(&baseline.snapshot).unwrap();
    let marker: Value = from_slice(&marker).unwrap();
    assert_eq!(baseline.seq, stored_seq);
    assert_eq!(snapshot["seq"], stored_seq);
    assert_eq!(snapshot["guild_id"], "100");
    assert!(baseline.topics.guild.owner_id == id(300));
    assert_eq!(
        marker,
        json!({
            "v": 1, "guild_id": "100", "seq": stored_seq,
            "occurred_at": snapshot["observed_at"],
            "ops": [{"topic": "guild", "op": "upsert", "id": "100",
                "value": snapshot["topics"]["guild"]}],
        }),
    );
    assert_eq!(
        recovery
            .publish(&generation, 43, now(), topics(guild_id), unexpected_write)
            .await,
        Err(PublishError::AlreadyPublished),
    );

    let replacement = recovery.begin();
    assert!(recovery.baseline().is_none());
    assert!(recovery.is_current(&replacement));
    assert!(!recovery.is_current(&generation));
    assert_eq!(
        recovery
            .publish(&generation, 43, now(), topics(guild_id), unexpected_write)
            .await,
        Err(PublishError::StaleGeneration),
    );
}

#[tokio::test]
async fn rejects_an_invalid_baseline_and_an_unrepresentable_gap_without_writing() {
    let guild_id = id(100);
    let mut recovery = Recovery::new(guild_id);
    let generation = recovery.begin();
    let mut invalid = topics(guild_id);
    invalid.members.clear();
    assert_eq!(
        recovery
            .publish(&generation, 0, now(), invalid, unexpected_write)
            .await,
        Err(PublishError::InvalidBaseline),
    );
    for expected in [MAX_SEQ - 1, u64::MAX] {
        assert_eq!(
            recovery
                .publish(
                    &generation,
                    expected,
                    now(),
                    topics(guild_id),
                    unexpected_write
                )
                .await,
            Err(PublishError::SequenceExhausted),
        );
    }
    assert!(recovery.baseline().is_none());
    assert!(recovery
        .publish(
            &generation,
            MAX_SEQ - 2,
            now(),
            topics(guild_id),
            async |_, _, seq| {
                assert_eq!(seq, MAX_SEQ);
                Ok::<_, Infallible>(())
            },
        )
        .await
        .is_ok());
    assert_eq!(recovery.baseline().unwrap().seq, MAX_SEQ);
}

#[tokio::test]
async fn a_failed_or_cancelled_reservation_keeps_recovery_unavailable() {
    let guild_id = id(100);
    let mut recovery = Recovery::new(guild_id);
    let generation = recovery.begin();
    assert_eq!(
        recovery
            .publish(&generation, 4, now(), topics(guild_id), async |_, _, _| {
                Err("not committed")
            })
            .await,
        Err(PublishError::Persist("not committed")),
    );
    assert!(recovery.baseline().is_none());

    // Model a commit whose response was lost before the caller observed it.
    let mut stored_seq = 4;
    tokio::select! {
        biased;
        _ = recovery.publish(&generation, 4, now(), topics(guild_id), async |_, _, next| {
            stored_seq = next;
            pending::<Result<(), Infallible>>().await
        }) => panic!("reservation should still be pending"),
        _ = async {} => {},
    }
    assert_eq!(stored_seq, 6);
    assert!(recovery.baseline().is_none());
    assert!(recovery
        .publish(
            &generation,
            stored_seq,
            now(),
            topics(guild_id),
            async |_, expected, next| {
                assert_eq!((expected, next), (6, 8));
                Ok::<_, Infallible>(())
            }
        )
        .await
        .is_ok());
    assert_eq!(recovery.baseline().unwrap().seq, 8);
}
