//! Protected, failing-first current-channel discovery contract. Tests only.
//! The paired implementation consumes this file unchanged. No live Discord.
#![cfg(feature = "db")]
#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_EVENTS, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const START: &str = "2099-01-02T03:04:05Z";
const START_EPOCH: &str = "4071006245";
const EMPTY: &str = "No open game posts in this channel.";
const INVALID_PAGE: &str = "This list page is invalid or stale. Run /lfg-list again.";
const UNAVAILABLE: &str = "Unable to list game posts. Try /lfg-list again.";
const PAGE_ROWS: usize = 5;

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        announcements: true,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn invocation(id: u64, guild: Option<&str>, channel: &str, cursor: Option<&str>) -> Interaction {
    let options = cursor.map_or_else(Vec::new, |cursor| {
        vec![json!({"type": 3, "name": "cursor", "value": cursor})]
    });
    let mut wire = json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "guild_id": guild, "channel": {"id": channel, "type": 0},
        "member": {"user": {"id": "3333", "username": "host", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false,
            "flags": 0, "permissions": PERM_MANAGE_EVENTS.to_string()},
        "token": "discovery-fixture-token", "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "lfg-list", "type": 1, "options": options}
    });
    if guild.is_none() {
        wire.as_object_mut()
            .expect("interaction object")
            .remove("guild_id");
        wire["user"] = wire["member"]["user"].clone();
        wire.as_object_mut()
            .expect("interaction object")
            .remove("member");
    }
    serde_json::from_value(wire).expect("synthetic slash interaction")
}

fn with_permissions(event: Interaction, permissions: u64) -> Interaction {
    let mut wire = serde_json::to_value(event).expect("wire event");
    wire["member"]["permissions"] = json!(permissions.to_string());
    serde_json::from_value(wire).expect("permissions fixture")
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, gates: RouterGates) -> InteractionRuntime {
    InteractionRuntime::new(
        gates,
        pool,
        ActionExecutor::with_proxy("discovery-fixture-token".into(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
        two_bot_core::ClassifierConfig::default(),
    )
}

async fn with_database<F>(case: F)
where
    F: for<'a> FnOnce(&'a sqlx::PgPool) -> BoxFuture<'a, ()>,
{
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    // Assertion failures must not abandon a migrated fixture at runtime shutdown.
    let result = AssertUnwindSafe(case(db.pool())).catch_unwind().await;
    db.close().await.expect("explicit fixture teardown");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn seed(
    pool: &sqlx::PgPool,
    id: &str,
    guild: &str,
    channel: &str,
    message: Option<&str>,
    title: &str,
) {
    sqlx::query(
        "INSERT INTO lfg_posts
         (id, guild_id, channel_id, message_id, title, starts_at, status, created_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz, 'open', '987654321', '2020-01-01T00:00:00Z')",
    )
    .bind(id)
    .bind(guild)
    .bind(channel)
    .bind(message)
    .bind(title)
    .bind(START)
    .execute(pool)
    .await
    .expect("seed persisted post");
}

async fn seed_pages(pool: &sqlx::PgPool, count: usize) {
    // Reverse insertion plus identical timestamps makes the ID tie-break observable.
    // Direct fixture inserts intentionally exceed today's creation quota in stress cases.
    for n in (0..count).rev() {
        seed(
            pool,
            &format!("p-{n:02}"),
            GUILD,
            CHANNEL,
            Some(&format!("{}", 8000 + n)),
            &format!("Game {n:02}"),
        )
        .await;
    }
}

async fn seed_private_signup(pool: &sqlx::PgPool) {
    sqlx::query("INSERT INTO lfg_roles (lfg_id,role_key,label,slots,position) VALUES ('p-00','tank','PRIVATE_ROLE_BODY',9,0)")
        .execute(pool).await.expect("seed private role");
    sqlx::query("INSERT INTO lfg_signups (lfg_id,user_id,role_key,joined_at) VALUES ('p-00','12345678987654321','tank','2020-01-02T03:04:05Z')")
        .execute(pool).await.expect("seed private signup");
}

async fn snapshot(pool: &sqlx::PgPool) -> Value {
    // MVCC versions catch UPDATE-to-the-same-value as well as changed contents.
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'posts', (SELECT jsonb_agg(to_jsonb(p) || jsonb_build_object('_xmin',p.xmin::text) ORDER BY p.id) FROM lfg_posts p),
          'roles', (SELECT jsonb_agg(to_jsonb(r) || jsonb_build_object('_xmin',r.xmin::text) ORDER BY r.lfg_id,r.role_key) FROM lfg_roles r),
          'signups', (SELECT jsonb_agg(to_jsonb(s) || jsonb_build_object('_xmin',s.xmin::text) ORDER BY s.lfg_id,s.user_id) FROM lfg_signups s),
          'audit', (SELECT jsonb_agg(to_jsonb(a) || jsonb_build_object('_xmin',a.xmin::text) ORDER BY a.id) FROM announcements_audit_log a))",
    ).fetch_one(pool).await.expect("read-only state snapshot")
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("Discord JSON")
}

struct Reply {
    handled: bool,
    requests: Vec<RestRequest>,
    content: String,
}

async fn execute(pool: sqlx::PgPool, gates: RouterGates, event: Interaction) -> Reply {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let rt = runtime(pool, &mock, gates);
    let handled = rt
        .handle(&event)
        .await
        .expect("runtime must return a truthful reply, not a transport error");
    let requests = mock.requests();
    let content = requests
        .last()
        .map(body)
        .and_then(|v| {
            v.get("content")
                .or_else(|| v.get("data").and_then(|d| d.get("content")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    drop(rt);
    mock.shutdown().await;
    Reply {
        handled,
        requests,
        content,
    }
}

fn assert_reply_only(reply: &Reply) {
    assert!(
        reply.handled,
        "/lfg-list must be registered and dispatched by the shared runtime"
    );
    assert!(
        !reply.requests.is_empty(),
        "the host needs an explicit ephemeral result"
    );
    assert!(
        reply.requests.len() <= 2,
        "one callback plus at most one reply edit"
    );
    for (index, request) in reply.requests.iter().enumerate() {
        let payload = body(request);
        if index == 0 {
            assert_eq!(request.method, "POST");
            assert!(
                request.path.starts_with("/api/v10/interactions/")
                    && request.path.ends_with("/callback")
            );
            assert_eq!(payload["data"]["flags"], 64, "discovery is ephemeral");
            assert!([4, 5].contains(&payload["type"].as_u64().expect("callback type")));
            if payload["type"] == 4 {
                assert_eq!(payload["data"]["allowed_mentions"]["parse"], json!([]));
            }
        } else {
            assert_eq!(request.method, "PATCH");
            assert_eq!(
                request.path,
                "/api/v10/webhooks/1111/discovery-fixture-token/messages/@original"
            );
            assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        }
        assert!(
            !request.path.contains("/channels/"),
            "no channel writes OR history scans"
        );
    }
    assert!(
        reply.content.encode_utf16().count() <= 2000,
        "Discord content ceiling"
    );
    for private in [
        "PRIVATE_ROLE_BODY",
        "12345678987654321",
        "987654321",
        "HIDDEN_OTHER_CHANNEL",
        "HIDDEN_OTHER_GUILD",
    ] {
        assert!(
            !reply.content.contains(private),
            "private state leaked: {private}"
        );
    }
}

async fn read(
    pool: &sqlx::PgPool,
    id: u64,
    guild: &str,
    channel: &str,
    cursor: Option<&str>,
) -> Reply {
    let before = snapshot(pool).await;
    let mut active = gates();
    active.configured_guild = Some(guild.parse().expect("guild snowflake"));
    let reply = execute(
        pool.clone(),
        active,
        invocation(id, Some(guild), channel, cursor),
    )
    .await;
    let after = snapshot(pool).await;
    assert_eq!(
        before, after,
        "listing must not touch posts, signups, roles, closure or audit receipts"
    );
    assert_reply_only(&reply);
    reply
}

fn ids(content: &str) -> Vec<String> {
    content
        .split('`')
        .enumerate()
        .filter(|&(n, part)| {
            n % 2 == 1 && part.starts_with("p-") && !part.contains(char::is_whitespace)
        })
        .map(|(_, part)| part.to_owned())
        .collect()
}

fn cursor(content: &str) -> Option<String> {
    content.split_once("/lfg-list cursor:").map(|(_, tail)| {
        let token = tail
            .split(|c: char| c == '`' || c.is_whitespace())
            .next()
            .expect("cursor token");
        assert!(
            !token.is_empty() && token.len() <= 512,
            "usable bounded continuation"
        );
        assert!(
            token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
            "copyable opaque token"
        );
        token.to_owned()
    })
}

fn assert_rows(reply: &Reply, expected: &[&str]) {
    assert_eq!(
        ids(&reply.content),
        expected,
        "whole visible rows in stable creation/ID order: {}",
        reply.content
    );
    for id in expected {
        let line = reply
            .content
            .lines()
            .find(|line| line.contains(&format!("`{id}`")))
            .expect("one complete row");
        assert!(
            line.contains("https://discord.com/channels/2222/4444/"),
            "validated current-channel jump link"
        );
        assert!(
            line.contains(&format!("<t:{START_EPOCH}:f>")),
            "localized start time, not a fabricated date"
        );
    }
}

#[tokio::test]
async fn empty_is_an_explicit_ephemeral_success_not_an_unhandled_command() {
    with_database(|pool| {
        Box::pin(async move {
            let reply = read(pool, 6001, GUILD, CHANNEL, None).await;
            assert_eq!(reply.content, EMPTY);
            assert!(cursor(&reply.content).is_none());
        })
    })
    .await;
}

#[tokio::test]
async fn one_confirmed_post_shows_management_id_title_local_time_and_exact_jump_link() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 1).await;
            seed_private_signup(pool).await;
            let reply = read(pool, 6002, GUILD, CHANNEL, None).await;
            assert_rows(&reply, &["p-00"]);
            assert!(reply.content.contains("Game 00"));
            assert!(reply
                .content
                .contains("https://discord.com/channels/2222/4444/8000"));
            assert!(reply.content.contains("End of list."));
            assert!(cursor(&reply.content).is_none());
        })
    })
    .await;
}

#[tokio::test]
async fn five_rows_are_a_complete_page_without_a_spurious_continuation() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, PAGE_ROWS).await;
            let reply = read(pool, 6003, GUILD, CHANNEL, None).await;
            assert_rows(&reply, &["p-00", "p-01", "p-02", "p-03", "p-04"]);
            assert!(cursor(&reply.content).is_none());
            assert!(reply.content.contains("End of list."));
        })
    })
    .await;
}

#[tokio::test]
async fn equal_timestamps_use_id_ties_and_all_pages_cover_each_visible_post_once() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 12).await;
            let mut next = None;
            let mut found = Vec::new();
            for page in 0..3 {
                let reply = read(pool, 6100 + page, GUILD, CHANNEL, next.as_deref()).await;
                let rows = ids(&reply.content);
                assert_eq!(rows.len(), if page < 2 { 5 } else { 2 });
                found.extend(rows);
                next = cursor(&reply.content);
                assert_eq!(next.is_some(), page < 2);
            }
            assert_eq!(
                found,
                (0..12).map(|n| format!("p-{n:02}")).collect::<Vec<_>>()
            );
        })
    })
    .await;
}

#[tokio::test]
async fn guild_and_current_channel_filter_before_paging_and_do_not_leak_counts() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 7).await;
            let before_foreign_rows = read(pool, 6200, GUILD, CHANNEL, None).await;
            let before_token = cursor(&before_foreign_rows.content).expect("initial continuation");
            for n in 0..30 {
                seed(
                    pool,
                    &format!("hidden-channel-{n}"),
                    GUILD,
                    "5555",
                    Some("9999"),
                    "HIDDEN_OTHER_CHANNEL",
                )
                .await;
                seed(
                    pool,
                    &format!("hidden-guild-{n}"),
                    "6666",
                    CHANNEL,
                    Some("9999"),
                    "HIDDEN_OTHER_GUILD",
                )
                .await;
            }
            let first = read(pool, 6201, GUILD, CHANNEL, None).await;
            assert_rows(&first, &["p-00", "p-01", "p-02", "p-03", "p-04"]);
            let token = cursor(&first.content).expect("continuation for two remaining rows");
            let second = read(pool, 6202, GUILD, CHANNEL, Some(&token)).await;
            assert_rows(&second, &["p-05", "p-06"]);
            assert!(cursor(&second.content).is_none());
            assert_eq!(
                before_foreign_rows
                    .content
                    .replace(&before_token, "<cursor>"),
                first.content.replace(&token, "<cursor>"),
                "foreign inserts cannot affect visible rows, counts, or continuation copy"
            );
            let empty = read(pool, 6203, GUILD, "7777", None).await;
            assert_eq!(
                empty.content, EMPTY,
                "no other-channel count or continuation metadata"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn closed_and_unconfirmed_or_delivery_uncertain_posts_are_not_discoverable() {
    with_database(|pool| Box::pin(async move {
        seed_pages(pool, 1).await;
        seed(pool, "hidden-closed", GUILD, CHANNEL, Some("8001"), "closed private body").await;
        sqlx::query("UPDATE lfg_posts SET status='closed',closed_at='2020-01-02T00:00:00Z' WHERE id='hidden-closed'")
            .execute(pool).await.expect("closed fixture");
        seed(pool, "hidden-unconfirmed", GUILD, CHANNEL, None, "unconfirmed private body").await;
        seed(pool, "hidden-uncertain", GUILD, CHANNEL, None, "uncertain private body").await;
        let reply = read(pool, 6301, GUILD, CHANNEL, None).await;
        assert_rows(&reply, &["p-00"]);
        for excluded in ["hidden-", "private body"] { assert!(!reply.content.contains(excluded)); }
        assert!(cursor(&reply.content).is_none());
    })).await;
}

#[tokio::test]
async fn past_open_posts_remain_visible_without_inventing_an_expiry_policy() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 1).await;
            sqlx::query("UPDATE lfg_posts SET starts_at='2020-01-02T03:04:05Z' WHERE id='p-00'")
                .execute(pool)
                .await
                .expect("past start");
            let reply = read(pool, 6302, GUILD, CHANNEL, None).await;
            assert_eq!(ids(&reply.content), ["p-00"]);
            assert!(reply.content.contains("<t:1577934245:f>"));
            assert!(
                reply.content.to_lowercase().contains("past"),
                "a past start is not upcoming or automatically closed"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn unreadable_start_has_an_unknown_label_instead_of_epoch_zero_or_empty_success() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 1).await;
            // PostgreSQL permits infinity even though OffsetDateTime cannot represent it.
            // This is malformed persisted input, not a request for a schema migration.
            sqlx::query("UPDATE lfg_posts SET starts_at='infinity'::timestamptz WHERE id='p-00'")
                .execute(pool)
                .await
                .expect("unknown start fixture");
            let reply = read(pool, 6303, GUILD, CHANNEL, None).await;
            assert_eq!(ids(&reply.content), ["p-00"]);
            assert!(reply.content.to_lowercase().contains("unknown"));
            assert!(!reply.content.contains("<t:0:"));
            assert!(!reply.content.contains("<t:"));
        })
    })
    .await;
}

#[tokio::test]
async fn invalid_stored_message_ids_never_become_discord_jump_links() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 1).await;
            for (n, message) in [
                "0",
                "08001",
                "+8001",
                "18446744073709551616",
                "8001/../../@everyone",
                "<@123>",
            ]
            .iter()
            .enumerate()
            {
                seed(
                    pool,
                    &format!("hidden-invalid-{n}"),
                    GUILD,
                    CHANNEL,
                    Some(message),
                    "invalid identity private body",
                )
                .await;
            }
            let reply = read(pool, 6304, GUILD, CHANNEL, None).await;
            assert_rows(&reply, &["p-00"]);
            assert!(!reply.content.contains("hidden-invalid"));
            assert!(!reply.content.contains("@everyone"));
            assert!(cursor(&reply.content).is_none());
        })
    })
    .await;
}

#[tokio::test]
async fn hostile_titles_are_inert_and_preserved_as_whole_rows_without_silent_truncation() {
    with_database(|pool| Box::pin(async move {
        let hostile = "@everyone <@123> <@&456> **bold** `code` [bait](https://evil.invalid)\n# heading 🦀";
        seed(pool, "p-00", GUILD, CHANNEL, Some("8000"), hostile).await;
        let unicode = "🦀".repeat(50); // exactly 100 UTF-16 units, the existing title limit
        seed(pool, "p-01", GUILD, CHANNEL, Some("8001"), &unicode).await;
        let reply = read(pool, 6401, GUILD, CHANNEL, None).await;
        assert_rows(&reply, &["p-00", "p-01"]);
        for raw in ["@everyone", "<@123>", "<@&456>", "**bold**", "`code`", "[bait](https://evil.invalid)", "\n# heading"] {
            assert!(!reply.content.contains(raw), "active mention/markdown survived: {raw}");
        }
        assert!(reply.content.contains("bold") && reply.content.contains("code") && reply.content.contains("heading"));
        assert_eq!(reply.content.matches('🦀').count(), 51, "do not silently shorten names");
    })).await;
}

#[tokio::test]
async fn maximum_titles_and_long_management_ids_use_explicit_pages_not_truncated_rows() {
    with_database(|pool| {
        Box::pin(async move {
            let mut expected = Vec::new();
            for n in 0..9 {
                let id = format!("p-{n:02}-{}", "x".repeat(123));
                let title = format!("{n:02}{}", "*".repeat(98));
                seed(
                    pool,
                    &id,
                    GUILD,
                    CHANNEL,
                    Some(&format!("{}", 8000 + n)),
                    &title,
                )
                .await;
                expected.push(id);
            }
            let mut next = None;
            let mut seen = Vec::new();
            for page in 0..9 {
                let reply = read(pool, 6500 + page, GUILD, CHANNEL, next.as_deref()).await;
                let rows = ids(&reply.content);
                assert!(!rows.is_empty() && rows.len() <= PAGE_ROWS);
                for id in &rows {
                    assert_eq!(id.len(), 128, "management ID cannot be shortened");
                    let line = reply
                        .content
                        .lines()
                        .find(|line| line.contains(&format!("`{id}`")))
                        .expect("whole row");
                    assert_eq!(
                        line.matches('*').count(),
                        98,
                        "title cannot be silently shortened"
                    );
                    assert!(
                        line.contains(&format!("<t:{START_EPOCH}:f>"))
                            && line.contains("https://discord.com/channels/2222/4444/")
                    );
                }
                seen.extend(rows);
                next = cursor(&reply.content);
                if next.is_none() {
                    break;
                }
            }
            assert!(next.is_none(), "bounded progress to end");
            assert_eq!(seen, expected);
        })
    })
    .await;
}

#[tokio::test]
async fn continuation_rejects_a_different_channel_or_guild_without_disclosing_a_row() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 6).await;
            let first = read(pool, 6601, GUILD, CHANNEL, None).await;
            let token = cursor(&first.content).expect("next-page token");
            for (id, guild, channel) in [(6602, GUILD, "5555"), (6603, "6666", CHANNEL)] {
                let refused = read(pool, id, guild, channel, Some(&token)).await;
                assert_eq!(refused.content, INVALID_PAGE);
                assert!(ids(&refused.content).is_empty());
                assert!(!refused.content.contains(&token));
            }
        })
    })
    .await;
}

#[tokio::test]
async fn forged_cursor_is_rejected_even_when_it_looks_like_a_real_token() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 6).await;
            let first = read(pool, 6701, GUILD, CHANNEL, None).await;
            let token = cursor(&first.content).expect("next-page token");
            let mut forged = token.into_bytes();
            let at = forged.len() / 2;
            forged[at] = if forged[at] == b'A' { b'B' } else { b'A' };
            let forged = String::from_utf8(forged).expect("ASCII token");
            let reply = read(pool, 6702, GUILD, CHANNEL, Some(&forged)).await;
            assert_eq!(reply.content, INVALID_PAGE);
            assert!(ids(&reply.content).is_empty());
        })
    })
    .await;
}

#[tokio::test]
async fn insert_and_close_between_pages_do_not_skip_unseen_rows_or_cross_visibility() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 8).await;
            let first = read(pool, 6801, GUILD, CHANNEL, None).await;
            assert_rows(&first, &["p-00", "p-01", "p-02", "p-03", "p-04"]);
            let token = cursor(&first.content).expect("next page");
            sqlx::query(
                "UPDATE lfg_posts SET status='closed',closed_at=now() WHERE id IN ('p-00','p-06')",
            )
            .execute(pool)
            .await
            .expect("concurrent close");
            seed(
                pool,
                "p-before",
                GUILD,
                CHANNEL,
                Some("8999"),
                "inserted before cursor",
            )
            .await;
            sqlx::query(
                "UPDATE lfg_posts SET created_at='2019-01-01T00:00:00Z' WHERE id='p-before'",
            )
            .execute(pool)
            .await
            .expect("earlier insertion");
            seed(
                pool,
                "p-hidden-after",
                GUILD,
                "5555",
                Some("8998"),
                "HIDDEN_OTHER_CHANNEL",
            )
            .await;
            let second = read(pool, 6802, GUILD, CHANNEL, Some(&token)).await;
            // A correctly bound cursor may recover explicitly when the data changed.
            if second.content == INVALID_PAGE {
                let restarted = read(pool, 6803, GUILD, CHANNEL, None).await;
                assert!(ids(&restarted.content).contains(&"p-before".to_owned()));
            } else {
                assert_rows(&second, &["p-05", "p-07"]);
                assert!(cursor(&second.content).is_none());
            }
        })
    })
    .await;
}

#[tokio::test]
async fn closing_all_remaining_rows_gives_end_or_stale_recovery_not_a_store_error() {
    with_database(|pool| {
        Box::pin(async move {
            seed_pages(pool, 6).await;
            let first = read(pool, 6901, GUILD, CHANNEL, None).await;
            let token = cursor(&first.content).expect("next page");
            sqlx::query("UPDATE lfg_posts SET status='closed',closed_at=now() WHERE id='p-05'")
                .execute(pool)
                .await
                .expect("last remaining post closed");
            let second = read(pool, 6902, GUILD, CHANNEL, Some(&token)).await;
            assert!(ids(&second.content).is_empty());
            assert!(second.content == INVALID_PAGE || second.content.contains("End of list."));
            assert!(cursor(&second.content).is_none());
        })
    })
    .await;
}

async fn closed_pool() -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("guarded lazy test endpoint");
    pool.close().await;
    pool
}

#[tokio::test]
async fn store_failure_is_not_empty_success_and_never_falls_back_to_discord_history() {
    let reply = execute(
        closed_pool().await,
        gates(),
        invocation(7001, Some(GUILD), CHANNEL, None),
    )
    .await;
    assert_reply_only(&reply);
    assert_eq!(reply.content, UNAVAILABLE);
    assert_ne!(reply.content, EMPTY);
    assert!(cursor(&reply.content).is_none());
}

#[tokio::test]
async fn malformed_and_oversized_pages_refuse_before_an_unavailable_store() {
    for (n, token) in [
        "",
        "-1",
        "999999999999999999999999999999",
        "../../5555",
        "<@123>",
        &"A".repeat(513),
    ]
    .iter()
    .enumerate()
    {
        let reply = execute(
            closed_pool().await,
            gates(),
            invocation(7100 + n as u64, Some(GUILD), CHANNEL, Some(token)),
        )
        .await;
        assert_reply_only(&reply);
        assert_eq!(
            reply.content, INVALID_PAGE,
            "invalid cursor must not reach SQL or leak its value"
        );
    }
}

#[tokio::test]
async fn existing_permissions_announcement_and_configured_guild_fences_precede_storage() {
    let pool = closed_pool().await;
    let missing_permission = execute(
        pool.clone(),
        gates(),
        with_permissions(invocation(7201, Some(GUILD), CHANNEL, None), 0),
    )
    .await;
    assert_reply_only(&missing_permission);
    assert_eq!(
        missing_permission.content,
        two_bot_core::MANAGE_EVENTS_REQUIRED
    );
    let mut disabled = gates();
    disabled.announcements = false;
    let reply = execute(
        pool.clone(),
        disabled,
        invocation(7202, Some(GUILD), CHANNEL, None),
    )
    .await;
    assert_reply_only(&reply);
    assert_eq!(reply.content, two_bot_core::ANNOUNCEMENTS_DISABLED_REPLY);
    let foreign = execute(
        pool.clone(),
        gates(),
        invocation(7203, Some("6666"), CHANNEL, None),
    )
    .await;
    assert!(
        foreign.requests.is_empty(),
        "wrong guild is ignored before callback/store"
    );
    let dm = execute(pool, gates(), invocation(7204, None, CHANNEL, None)).await;
    assert!(
        dm.requests.is_empty(),
        "DM is ignored before callback/store"
    );
}

#[tokio::test]
async fn published_command_and_compiled_help_explain_discovery_without_granting_roles() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(closed_pool().await, &mock, gates());
    let defs = rt
        .router
        .publish_set(&[])
        .expect("live command definitions");
    let value = serde_json::to_value(&defs).expect("registry JSON");
    let command = value
        .as_array()
        .expect("command set")
        .iter()
        .find(|v| v["name"] == "lfg-list")
        .expect("/lfg-list must be discoverable alongside post/close");
    assert_eq!(
        command["description"],
        "List open game posts in this channel"
    );
    assert_eq!(
        command["default_member_permissions"],
        PERM_MANAGE_EVENTS.to_string()
    );
    assert_eq!(command["options"].as_array().expect("options").len(), 1);
    assert_eq!(command["options"][0]["name"], "cursor");
    assert_eq!(command["options"][0]["type"], 3);
    assert_eq!(command["options"][0]["required"], false);
    assert_eq!(command["options"][0]["max_length"], 512);
    let help = two_bot_core::help::help_text(&defs);
    assert!(
        help.contains("/lfg-list"),
        "compiled help must advertise the read path"
    );
    assert!(help.contains("Manage Events"));
    assert!(mock.requests().is_empty());
    drop(rt);
    mock.shutdown().await;
}

#[tokio::test]
async fn store_selection_has_a_sql_bound_and_never_materializes_foreign_channel_titles() {
    with_database(|pool| Box::pin(async move {
        seed_pages(pool, 19).await;
        for n in 0..40 {
            seed(pool, &format!("hidden-{n}"), GUILD, "5555", Some("9999"), "HIDDEN_OTHER_CHANNEL").await;
        }
        let before = snapshot(pool).await;
        // A projection guard observes the SQL actually executed against PostgreSQL.
        // Unlike grepping Rust source, it rejects a whole-guild fetch followed by
        // Rust filtering, an unbounded fetch_all, and an OFFSET query at runtime.
        // Session settings are instrumentation only; no probe or domain row writes.
        // STABLE lets PostgreSQL defer title projection until after ORDER BY/LIMIT.
        // VOLATILE would force this view to project every candidate before sorting,
        // falsely rejecting even the bounded read-only control below.
        sqlx::query("CREATE FUNCTION lfg_list_projection_guard(g text, c text, title text) RETURNS text LANGUAGE plpgsql STABLE AS $$
            DECLARE q text := lower(current_query()); stamp text := statement_timestamp()::text; n int;
            BEGIN
              IF g <> '2222' OR c <> '4444' OR position('limit' in q) = 0
                 OR position('offset' in q) > 0 OR position('where' in q) = 0
                 OR position('guild_id' in q) = 0 OR position('channel_id' in q) = 0 THEN
                RAISE EXCEPTION 'unbounded or unfenced LFG read';
              END IF;
              IF current_setting('lfg_acceptance.stamp', true) IS DISTINCT FROM stamp THEN
                PERFORM set_config('lfg_acceptance.stamp', stamp, false);
                PERFORM set_config('lfg_acceptance.projected', '0', false);
              END IF;
              n := current_setting('lfg_acceptance.projected')::int + 1;
              PERFORM set_config('lfg_acceptance.projected', n::text, false);
              IF n > 6 THEN RAISE EXCEPTION 'more than one bounded page plus lookahead materialized'; END IF;
              RETURN title;
            END $$")
            .execute(pool).await.expect("projection guard fixture");
        sqlx::query("ALTER TABLE lfg_posts RENAME TO lfg_posts_acceptance_fixture")
            .execute(pool).await.expect("private fixture table alias");
        sqlx::query("CREATE VIEW lfg_posts AS SELECT id,guild_id,channel_id,message_id,
            lfg_list_projection_guard(guild_id,channel_id,title) AS title,
            starts_at,status,created_by,created_at,closed_at FROM lfg_posts_acceptance_fixture")
            .execute(pool).await.expect("instrumented read boundary");
        let mut readonly = pool.begin().await.expect("read-only oracle control");
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *readonly).await.expect("read-only transaction");
        let control: Vec<(String, String)> = sqlx::query_as("SELECT id,title FROM lfg_posts WHERE guild_id=$1 AND channel_id=$2 ORDER BY created_at,id LIMIT $3")
            .bind(GUILD).bind(CHANNEL).bind(6_i64).fetch_all(&mut *readonly).await.expect("guard accepts bounded read-only SQL");
        assert_eq!(control.len(), 6);
        readonly.rollback().await.expect("close read-only control");
        for bad in [
            "SELECT title FROM lfg_posts WHERE guild_id='2222' AND channel_id='4444'",
            "SELECT title FROM lfg_posts WHERE guild_id='2222' AND channel_id='4444' LIMIT 7",
            "SELECT title FROM lfg_posts WHERE guild_id='2222' AND channel_id='4444' LIMIT 6 OFFSET 1",
            "SELECT title FROM lfg_posts WHERE guild_id='2222' AND channel_id='5555' LIMIT 6",
        ] {
            let error = sqlx::query(bad).fetch_all(pool).await.expect_err("guard must catch the unsafe query mutation");
            assert_eq!(error.as_database_error().and_then(|e| e.code()).as_deref(), Some("P0001"));
        }
        let reply = execute(pool.clone(), gates(), invocation(7301, Some(GUILD), CHANNEL, None)).await;
        sqlx::query("DROP VIEW lfg_posts").execute(pool).await.expect("remove private probe view");
        sqlx::query("ALTER TABLE lfg_posts_acceptance_fixture RENAME TO lfg_posts")
            .execute(pool).await.expect("restore fixture name");
        assert_eq!(before, snapshot(pool).await, "selection cannot mutate durable LFG state");
        assert_reply_only(&reply);
        assert_rows(&reply, &["p-00", "p-01", "p-02", "p-03", "p-04"]);
        assert!(cursor(&reply.content).is_some());
    })).await;
}
