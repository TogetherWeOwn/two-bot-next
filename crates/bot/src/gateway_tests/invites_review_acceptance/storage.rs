#![cfg(test)]

use serde_json::json;
use twilight_gateway::{Event, EventTypeFlags};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::{InviteState, NoopFacts, NoopLeveling};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{NoClassification, Pipeline, ScriptedInvites};

use super::support::*;
use crate::gateway_tests::checkpoint;
use crate::invites_command::current_inviter;

type Journey = Pipeline<GatewayFunnelBuffer, NoopLeveling, NoopFacts, ScriptedInvites>;

async fn dispatch(
    f: &Fixture,
    pipeline: &Journey,
    seq: u64,
    kind: &str,
    member: u64,
    joined_at: &str,
    receipt: &str,
) {
    let mut data = json!({
        "guild_id": GUILD.to_string(),
        "user": {"id": member.to_string(), "username": "fixture-member", "discriminator": "0", "bot": false}
    });
    if kind == "GUILD_MEMBER_ADD" {
        data["roles"] = json!([]);
        data["joined_at"] = json!(joined_at.replace('Z', "+00:00"));
        data["pending"] = json!(false);
        data["deaf"] = json!(false);
        data["mute"] = json!(false);
        data["flags"] = json!(0);
    }
    let packet = json!({"op": 0, "s": seq, "t": kind, "d": data}).to_string();
    let parsed = twilight_gateway::parse(packet, EventTypeFlags::all())
        .expect("packet parses")
        .expect("known dispatch");
    pipeline.handle_at(&Event::from(parsed), receipt);
    GatewaySessionStore::new(f.db.pool().clone(), GUILD.to_string(), 0)
        .commit_dispatch(
            &checkpoint("invite-review-replay", seq, "ws://fixture"),
            pipeline.handlers().store().take_batch(),
        )
        .await
        .expect("journal replay in gateway order");
}

fn counters(alice: u64, bram: u64) -> Vec<InviteState> {
    [("alice", alice, ALICE), ("bram", bram, BRAM)]
        .into_iter()
        .map(|(code, uses, inviter)| InviteState {
            code: code.into(),
            uses,
            inviter_id: Some(inviter),
            channel_id: Some(4444),
            max_uses: 0,
            temporary: false,
        })
        .collect()
}

async fn replay(f: &Fixture) {
    let pipeline = Pipeline::new(
        GatewayFunnelBuffer::default(),
        None,
        None,
        ScriptedInvites::new(),
        NoClassification,
    );
    pipeline.invite_source().push(GUILD, counters(0, 0));
    dispatch(
        f,
        &pipeline,
        1,
        "GUILD_MEMBER_ADD",
        9555,
        "2026-09-30T11:59:59.000Z",
        AT,
    )
    .await;
    pipeline.invite_source().push(GUILD, counters(1, 0));
    dispatch(f, &pipeline, 2, "GUILD_MEMBER_ADD", MEMBER, AT, AT).await;
    // A replayed removal has only receipt time. The subsequent replayed add
    // carries Discord's earlier joined_at, but is the final committed transition.
    dispatch(
        f,
        &pipeline,
        3,
        "GUILD_MEMBER_REMOVE",
        MEMBER,
        AT,
        "2026-09-30T12:00:05.000Z",
    )
    .await;
    pipeline.invite_source().push(GUILD, counters(1, 1));
    dispatch(
        f,
        &pipeline,
        4,
        "GUILD_MEMBER_ADD",
        MEMBER,
        "2026-09-30T12:00:03.000Z",
        "2026-09-30T12:00:06.000Z",
    )
    .await;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_type, to_char(occurred_at AT TIME ZONE 'UTC', 'HH24:MI:SS') FROM events
         WHERE member_id = $1 AND event_type IN ('member_join', 'member_leave') ORDER BY id",
    )
    .bind(MEMBER.to_string())
    .fetch_all(f.db.pool())
    .await
    .expect("inspect actual replay chronology");
    assert_eq!(
        rows,
        vec![
            ("member_join".into(), "12:00:00".into()),
            ("member_leave".into(), "12:00:05".into()),
            ("member_join".into(), "12:00:03".into()),
        ],
        "the fixture must reach a genuine timestamp inversion, not merely a tie"
    );
}

#[tokio::test]
async fn replayed_leave_receipt_does_not_erase_the_final_committed_rejoin_count() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            replay(f).await;
            let alice = f.command(ALICE).await;
            let bram = f.command(BRAM).await;
            assert_personal(&alice, "Alice", 0);
            assert_personal(&bram, "Bram", 1);
            assert_eq!(board(&bram), vec![("Bram".into(), 1)]);
        })
    })
    .await;
}

#[tokio::test]
async fn replayed_leave_receipt_does_not_hide_the_current_welcome_inviter() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            replay(f).await;
            let welcome = f.welcome(MEMBER).await;
            assert!(
                text(&welcome).ends_with("Invited by Bram."),
                "current inviter belongs to the final gateway transition: {:?}",
                text(&welcome)
            );
            assert!(!text(&welcome).contains("Alice"));
        })
    })
    .await;
}

const BAD_METADATA: [&str; 9] = [
    "{",
    "not json",
    "null",
    "true",
    "42",
    "[]",
    "[{\"inviterId\":\"9004\"}]",
    "\"9004\"",
    "{\"inviterId\":null}",
];

#[tokio::test]
async fn malformed_and_nonobject_metadata_cannot_poison_counts_for_a_valid_peer() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            for (index, metadata) in BAD_METADATA.iter().enumerate() {
                f.seed(9500 + index as u64, metadata).await;
            }
            f.credit(MEMBER, BRAM).await;
            let body = f.command(BRAM).await;
            assert_personal(&body, "Bram", 1);
            assert_eq!(board(&body), vec![("Bram".into(), 1)]);
        })
    })
    .await;
}

#[tokio::test]
async fn malformed_and_nonobject_metadata_are_unknown_in_both_current_and_welcome_reads() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            f.credit(MEMBER, BRAM).await;
            for (index, metadata) in BAD_METADATA.iter().enumerate() {
                let invitee = 9500 + index as u64;
                f.seed(invitee, metadata).await;
                assert_eq!(
                    current_inviter(f.db.pool(), GUILD, invitee)
                        .await
                        .expect("corrupt evidence is unknown, not a query failure"),
                    None,
                    "metadata {metadata:?}"
                );
                let welcome = f.welcome(invitee).await;
                assert!(
                    !text(&welcome).contains("Invited by"),
                    "no invented inviter from {metadata:?}"
                );
            }
            assert_eq!(
                current_inviter(f.db.pool(), GUILD, MEMBER)
                    .await
                    .expect("valid peer still readable"),
                Some(BRAM.to_string())
            );
            assert!(text(&f.welcome(MEMBER).await).ends_with("Invited by Bram."));
        })
    })
    .await;
}

async fn invalid_id(f: &Fixture, invitee: u64, inviter: &str) {
    f.seed(invitee, &json!({"inviterId": inviter}).to_string())
        .await;
    f.credit(MEMBER, BRAM).await;
    let body = f.command(invitee).await;
    assert_personal(&body, &format!("Person {invitee}"), 0);
    assert_eq!(
        board(&body),
        vec![("Bram".into(), 1)],
        "invalid attribution must not appear as a board row"
    );
    assert_eq!(
        current_inviter(f.db.pool(), GUILD, invitee)
            .await
            .expect("invalid inviter does not fail the reader"),
        None
    );
    let welcome = f.welcome(invitee).await;
    assert!(
        !text(&welcome).contains("Invited by"),
        "invalid inviter must not be welcomed as credit"
    );
    let peer = f.command(BRAM).await;
    assert_personal(&peer, "Bram", 1);
    assert!(text(&f.welcome(MEMBER).await).ends_with("Invited by Bram."));
}

#[tokio::test]
async fn self_inviter_is_rejected_by_counts_current_reader_and_welcome() {
    with_fixture(Discord::default(), |f| {
        Box::pin(invalid_id(f, 9500, "9500"))
    })
    .await;
}

#[tokio::test]
async fn zero_inviter_is_rejected_by_counts_current_reader_and_welcome() {
    with_fixture(Discord::default(), |f| Box::pin(invalid_id(f, 9500, "0"))).await;
}

#[tokio::test]
async fn leading_zero_inviter_is_rejected_by_counts_current_reader_and_welcome() {
    with_fixture(Discord::default(), |f| {
        Box::pin(invalid_id(f, 9500, "09004"))
    })
    .await;
}

#[tokio::test]
async fn one_above_u64_max_is_rejected_by_counts_current_reader_and_welcome() {
    with_fixture(Discord::default(), |f| {
        Box::pin(invalid_id(f, 9500, "18446744073709551616"))
    })
    .await;
}

#[tokio::test]
async fn u64_max_is_valid_in_counts_current_reader_and_welcome() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            f.credit(MEMBER, u64::MAX).await;
            f.credit(9500, BRAM).await;
            let name = "Person 18446744073709551615";
            let body = f.command(u64::MAX).await;
            assert_personal(&body, name, 1);
            assert_eq!(board(&body), vec![("Bram".into(), 1), (name.into(), 1)]);
            assert_eq!(
                current_inviter(f.db.pool(), GUILD, MEMBER)
                    .await
                    .expect("full u64 range"),
                Some("18446744073709551615".into())
            );
            assert!(
                text(&f.welcome(MEMBER).await).ends_with("Invited by Person 18446744073709551615.")
            );
        })
    })
    .await;
}

async fn cap(f: &Fixture, distinct: u64) {
    // Numeric tie-breaking differs from lexical order at 9999 -> 10000.
    // Two leaders tie at 3; the remaining inviter counts tie at 1.
    let ids = [
        9999, 10000, 10001, 10002, 10003, 10004, 10005, 10006, 10007, 10008, 10009, 10010,
    ];
    let mut invitee = 20000;
    for (index, id) in ids.iter().take(distinct as usize).enumerate().rev() {
        for _ in 0..if index < 2 { 3 } else { 1 } {
            f.credit(invitee, *id).await;
            invitee += 1;
        }
    }
    let target = if distinct == 12 { 10010 } else { 9555 };
    let body = f.command(target).await;
    assert_personal(
        &body,
        &format!("Person {target}"),
        if distinct == 12 { 1 } else { 0 },
    );
    let expected: Vec<_> = ids
        .iter()
        .take(distinct.min(10) as usize)
        .enumerate()
        .map(|(index, id)| (format!("Person {id}"), if index < 2 { 3 } else { 1 }))
        .collect();
    assert_eq!(
        board(&body),
        expected,
        "exact board membership, ordering and count at {distinct} inviters"
    );
    if distinct == 12 {
        assert!(
            !text(&body).contains("Person 10009"),
            "eleventh inviter is not on the board"
        );
        assert_eq!(
            text(&body).matches("Person 10010").count(),
            1,
            "personal row survives outside the ten-row board"
        );
    }
}

#[tokio::test]
async fn nine_distinct_inviters_all_appear_in_the_exact_board_order() {
    with_fixture(Discord::default(), |f| Box::pin(cap(f, 9))).await;
}

#[tokio::test]
async fn ten_distinct_inviters_fill_but_do_not_overflow_the_board() {
    with_fixture(Discord::default(), |f| Box::pin(cap(f, 10))).await;
}

#[tokio::test]
async fn twelve_distinct_inviters_keep_exactly_ten_board_rows_and_the_personal_row() {
    with_fixture(Discord::default(), |f| Box::pin(cap(f, 12))).await;
}
