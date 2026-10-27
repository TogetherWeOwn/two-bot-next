#![cfg(test)]
//! A second same-key boundary pair is still membership: join(t0), leave(t1),
//! rejoin(t0), leave(t1) with distinct gateway sequences ends absent. The
//! fourth delivery collides with the second's legacy key, so only the writer's
//! opposite-boundary recovery can admit it. Readers alone cannot repair a
//! transition that was never written, so the durable rows are pinned first.

use serde_json::json;
use twilight_gateway::{Event, EventTypeFlags};
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::DispatchAction;
use two_bot_core::{InviteState, NoopFacts, NoopLeveling};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{NoClassification, Pipeline, ScriptedInvites};

use super::support::*;
use crate::gateway_tests::checkpoint;
use crate::invites_command::current_inviter;

type Journey = Pipeline<GatewayFunnelBuffer, NoopLeveling, NoopFacts, ScriptedInvites>;

const SESSION: &str = "invite-final-leave-replay";
const JOINED: &str = AT;
const LEFT: &str = "2026-09-30T12:00:05.000Z";

fn packet(seq: u64, kind: &str, member: u64) -> Event {
    let mut data = json!({
        "guild_id": GUILD.to_string(),
        "user": {"id": member.to_string(), "username": "fixture-member", "discriminator": "0", "bot": false}
    });
    if kind == "GUILD_MEMBER_ADD" {
        data["roles"] = json!([]);
        data["joined_at"] = json!(JOINED.replace('Z', "+00:00"));
        data["pending"] = json!(false);
        data["deaf"] = json!(false);
        data["mute"] = json!(false);
        data["flags"] = json!(0);
    }
    let raw = json!({"op": 0, "s": seq, "t": kind, "d": data}).to_string();
    Event::from(
        twilight_gateway::parse(raw, EventTypeFlags::all())
            .expect("packet parses")
            .expect("known dispatch"),
    )
}

fn store(f: &Fixture) -> GatewaySessionStore {
    GatewaySessionStore::new(f.db.pool().clone(), GUILD.to_string(), 0)
}

/// Handle one gateway packet and commit its batch at `seq` of one session.
async fn dispatch(
    f: &Fixture,
    pipeline: &Journey,
    seq: u64,
    kind: &str,
    member: u64,
    receipt: &str,
) -> DispatchAction {
    pipeline.handle_at(&packet(seq, kind, member), receipt);
    store(f)
        .commit_dispatch(
            &checkpoint(SESSION, seq, "ws://fixture"),
            pipeline.handlers().store().take_batch(),
        )
        .await
        .expect("gateway batch commit")
}

fn bram_uses(uses: u64) -> Vec<InviteState> {
    vec![InviteState {
        code: "bram".into(),
        uses,
        inviter_id: Some(BRAM),
        channel_id: Some(4444),
        max_uses: 0,
        temporary: false,
    }]
}

async fn membership(f: &Fixture) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT event_type, to_char(occurred_at AT TIME ZONE 'UTC', 'HH24:MI:SS') FROM events
         WHERE guild_id = $1 AND member_id = $2
           AND event_type IN ('member_join', 'member_leave') ORDER BY id",
    )
    .bind(GUILD.to_string())
    .bind(MEMBER.to_string())
    .fetch_all(f.db.pool())
    .await
    .expect("inspect durable membership")
}

/// Every committed `events` row, and the journal's `events` evidence.
async fn evidence(f: &Fixture) -> (Vec<String>, Vec<String>) {
    let rows: Vec<i64> = sqlx::query_scalar("SELECT id FROM events ORDER BY id")
        .fetch_all(f.db.pool())
        .await
        .expect("inspect events");
    let journal = two_bot_store::journal::entries_since(f.db.pool(), "events", 0)
        .await
        .expect("inspect rollback journal")
        .into_iter()
        .map(|entry| {
            assert_eq!(entry.op, two_bot_store::journal::JournalOp::Insert);
            assert_eq!(entry.pre_image, None);
            entry.row_identity
        })
        .collect();
    (rows.iter().map(i64::to_string).collect(), journal)
}

async fn assert_checkpoint(f: &Fixture, seq: u64) {
    let saved = store(f)
        .load()
        .await
        .expect("read checkpoint")
        .expect("checkpoint saved");
    assert_eq!(
        (saved.session_id.as_str(), saved.sequence),
        (SESSION, seq),
        "checkpoint names the last applied dispatch"
    );
}

#[tokio::test]
async fn colliding_second_leave_after_colliding_rejoin_is_retained_and_ends_membership() {
    with_fixture(Discord::default(), |f| {
        Box::pin(async move {
            two_bot_store::migrate(f.db.pool())
                .await
                .expect("store migrations add the journal tables");
            let pipeline = Pipeline::new(
                GatewayFunnelBuffer::default(),
                None,
                None,
                ScriptedInvites::new(),
                NoClassification,
            );
            // Baseline counters through an unrelated member's join.
            pipeline.invite_source().push(GUILD, bram_uses(0));
            assert_eq!(
                dispatch(f, &pipeline, 1, "GUILD_MEMBER_ADD", 9555, AT).await,
                DispatchAction::Apply
            );
            pipeline.invite_source().push(GUILD, bram_uses(1));
            assert_eq!(
                dispatch(f, &pipeline, 2, "GUILD_MEMBER_ADD", MEMBER, AT).await,
                DispatchAction::Apply
            );
            assert_eq!(
                dispatch(f, &pipeline, 3, "GUILD_MEMBER_REMOVE", MEMBER, LEFT).await,
                DispatchAction::Apply
            );
            // Same joined_at and same leave receipt as the first pair: both
            // legacy keys collide, yet each is a distinct gateway delivery.
            pipeline.invite_source().push(GUILD, bram_uses(2));
            assert_eq!(
                dispatch(
                    f,
                    &pipeline,
                    4,
                    "GUILD_MEMBER_ADD",
                    MEMBER,
                    "2026-09-30T12:00:06.000Z"
                )
                .await,
                DispatchAction::Apply
            );
            assert_eq!(
                dispatch(f, &pipeline, 5, "GUILD_MEMBER_REMOVE", MEMBER, LEFT).await,
                DispatchAction::Apply
            );

            let expected = vec![
                ("member_join".to_owned(), "12:00:00".to_owned()),
                ("member_leave".to_owned(), "12:00:05".to_owned()),
                ("member_join".to_owned(), "12:00:00".to_owned()),
                ("member_leave".to_owned(), "12:00:05".to_owned()),
            ];
            assert_eq!(
                membership(f).await,
                expected,
                "every distinct opposite-boundary delivery is durable; the final leave \
                 must not be suppressed by the earlier leave's later timestamp"
            );
            assert_checkpoint(f, 5).await;
            let (rows, journal) = evidence(f).await;
            assert_eq!(rows.len(), 5, "four membership rows plus the baseline join");
            assert_eq!(
                journal, rows,
                "each committed row is journaled exactly once in commit order"
            );

            // Duplicate deliveries of the final leave and of the colliding
            // rejoin are checkpoint no-ops: no row, no journal entry, no move.
            assert_eq!(
                dispatch(f, &pipeline, 5, "GUILD_MEMBER_REMOVE", MEMBER, LEFT).await,
                DispatchAction::Duplicate
            );
            pipeline.invite_source().push(GUILD, bram_uses(2));
            assert_eq!(
                dispatch(
                    f,
                    &pipeline,
                    4,
                    "GUILD_MEMBER_ADD",
                    MEMBER,
                    "2026-09-30T12:00:06.000Z"
                )
                .await,
                DispatchAction::Duplicate
            );
            assert_eq!(membership(f).await, expected);
            assert_checkpoint(f, 5).await;
            assert_eq!(evidence(f).await, (rows.clone(), rows));

            assert_eq!(
                current_inviter(f.db.pool(), GUILD, MEMBER)
                    .await
                    .expect("current inviter reads"),
                None,
                "an absent member has no current inviter"
            );
            let bram = f.command(BRAM).await;
            assert_personal(&bram, "Bram", 0);
            assert_eq!(
                board(&bram),
                vec![],
                "no net credit survives the final leave"
            );
        })
    })
    .await;
}
