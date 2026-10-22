//! Protected acceptance: `/afk` status with mention replies.
//!
//! Pinned seam: `CommandRuntime::on_interaction` answers `/afk [message]`
//! (the note arrives as the string option named `message`) and
//! `CommandRuntime::on_message` answers mentions of AFK members plus the
//! auto-clear on the AFK member's next message. Mention fixtures put the
//! target in `Message.mentions` (server truth), never bare `<@id>` text
//! alone. Each test owns a disposable database and a loopback REST double;
//! no live Discord, no shared state.
//!
//! GREEN contract (the builder satisfies every line without editing this
//! file):
//! - `/afk [message]` answers exactly one immediate ephemeral callback
//!   (type 4, flags 64). The confirmation names the AFK state
//!   (case-insensitive `afk`) and echoes a supplied note verbatim. A second
//!   send replaces the stored note.
//! - A message mentioning an AFK member posts exactly one channel message
//!   carrying the note verbatim, an elapsed fragment (`<n> <unit>` seconds,
//!   minutes or hours, or the words `just now`), and the AFK state name.
//!   The reply never re-pings the AFK member: no `<@id>` mention syntax in
//!   the delivered content and no user-mention parsing or AFK-id listing in
//!   `allowed_mentions`.
//! - Mention replies are rate-limited per mentioner: an immediate repeat by
//!   the same member is suppressed while a different member still answers.
//! - Any member message by the AFK member clears the status and posts
//!   exactly one quiet confirmation (names `afk`, pings nobody: no `<@`,
//!   `@everyone` or `@here` in the content, `allowed_mentions.parse`
//!   absent or empty). Later mentions stay silent.
//! - An over-cap note (2000 chars) and hostile notes (`@everyone`, `@here`,
//!   `<@id>` mention syntax) are refused with exactly one immediate
//!   ephemeral callback that is owned (never the unknown-command literal),
//!   names the length bound (`too long`, case-insensitive) for the over-cap
//!   case, and never echoes the payload or any mention syntax. Refused
//!   notes store zero state: later mentions stay silent.
//! - Bot-authored messages and self-mentions never trigger an AFK reply.
//!
//! RED on baseline: `/afk` is not a builtin, so every set answers the
//! unknown-command literal (confirmation assertions fail); `on_message`
//! only serves sticky state, so mention and clear tests observe zero posts.
//! The bot/self silence controls already pass.
//!
//! Protected: the paired builder slice consumes this file read-only. All
//! fixture notes are digit-free so the elapsed matcher can only match the
//! rendered elapsed fragment, never the note.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY;
use two_bot_testsupport::TestDatabase;

const AFK_SETTER: u64 = 77;
const MENTIONER_A: u64 = 99;
const MENTIONER_B: u64 = 100;
const NOTE: &str = "grabbing lunch back soon";
const UPDATED_NOTE: &str = "in a meeting until three";

/// Disposable migrated database, or `None` with a vacuous skip when the
/// guarded test URL is absent (CI always supplies it).
async fn disposable_db(test: &str) -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable test database"),
    )
}

async fn afk_runtime(db: &TestDatabase, origin: String) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    )
}

/// `/afk [message]` from `member`: everyone may set their own status, so the
/// invoker holds no permissions. Each invocation carries a unique
/// interaction id and token.
fn afk_slash(interaction_id: u64, member: u64, note: Option<&str>) -> Interaction {
    let options = note
        .map(|text| {
            vec![option(
                "message",
                CommandOptionValue::String(text.to_owned()),
            )]
        })
        .unwrap_or_default();
    let mut interaction = from_member(
        slash("afk", Some(CHANNEL), options),
        member,
        Permissions::empty(),
    );
    interaction.id = Id::new(interaction_id);
    interaction.token = format!("afk-test-token-{interaction_id}");
    interaction
}

fn mention_message(id: u64, author: u64, author_bot: bool, mentioned: u64) -> Message {
    let mut msg = message(id, CHANNEL, author_bot, Some(GUILD));
    msg.author = user(author, author_bot);
    msg.content = format!("<@{mentioned}> are you there?");
    let mut mention = serde_json::to_value(user(mentioned, false)).expect("mention value");
    mention["public_flags"] = json!(0);
    msg.mentions = vec![serde_json::from_value(mention).expect("mention fixture")];
    msg
}

fn plain_message(id: u64, author: u64) -> Message {
    let mut msg = message(id, CHANNEL, false, Some(GUILD));
    msg.author = user(author, false);
    msg.content = "back at my desk".to_owned();
    msg
}

fn callback_body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("callback json")
}

fn post_body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("channel post json")
}

fn post_content(request: &RestRequest) -> String {
    post_body(request)["content"]
        .as_str()
        .expect("channel post text")
        .to_owned()
}

/// GREEN elapsed rendering: a `<n> <unit>` duration (seconds, minutes or
/// hours in short or long form) or the words `just now`.
fn contains_elapsed(content: &str) -> bool {
    let lower = content.to_lowercase();
    if lower.contains("just now") {
        return true;
    }
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k] == b' ' {
                k += 1;
            }
            let mut end = k;
            while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
                end += 1;
            }
            match &lower[k..end] {
                "s" | "sec" | "secs" | "second" | "seconds" | "m" | "min" | "mins" | "minute"
                | "minutes" | "h" | "hr" | "hrs" | "hour" | "hours" => return true,
                _ => {}
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// The AFK member is never re-pinged: no `<@id>` mention syntax in the
/// delivered content, and `allowed_mentions` (when present) parses no user
/// mentions and lists no AFK id.
fn assert_no_repings(body: &Value, afk_id: u64) {
    let content = body["content"].as_str().unwrap_or_default();
    assert!(
        !content.contains(&format!("<@{afk_id}>")) && !content.contains(&format!("<@!{afk_id}>")),
        "auto-reply must not re-ping the AFK member, got {content:?}"
    );
    let parse = body["allowed_mentions"]["parse"].clone();
    if !parse.is_null() {
        let parse = parse.as_array().expect("allowed_mentions.parse array");
        assert!(
            !parse.iter().any(|v| v == "users" || v == "everyone"),
            "auto-reply must not parse user mentions, got {parse:?}"
        );
    }
    let users = body["allowed_mentions"]["users"].clone();
    if !users.is_null() {
        let users = users.as_array().expect("allowed_mentions.users array");
        assert!(
            !users
                .iter()
                .any(|v| v == &json!(afk_id.to_string()) || v == &json!(afk_id)),
            "auto-reply must not list the AFK member, got {users:?}"
        );
    }
}

fn assert_ephemeral_confirmation(body: &Value, what: &str) -> String {
    assert_eq!(body["type"], 4, "{what} answers immediately, never a defer");
    assert_eq!(body["data"]["flags"], 64, "{what} is ephemeral");
    body["data"]["content"]
        .as_str()
        .expect("confirmation text")
        .to_owned()
}

#[tokio::test]
async fn afk_set_replies_confirmation_echoing_note() {
    let Some(db) = disposable_db("afk_set_replies_confirmation_echoing_note").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(701, AFK_SETTER, Some(NOTE)))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "one set confirmation, no deferral");
    let content = assert_ephemeral_confirmation(&callback_body(&callbacks[0]), "set");
    assert!(
        content.contains(NOTE),
        "confirmation echoes the note verbatim, got {content:?}"
    );
    assert!(
        content.to_lowercase().contains("afk"),
        "confirmation names the AFK state, got {content:?}"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_set_without_note_confirms_and_mentions_answer() {
    let Some(db) = disposable_db("afk_set_without_note_confirms_and_mentions_answer").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(702, AFK_SETTER, None))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "noteless set still confirms once");
    let content = assert_ephemeral_confirmation(&callback_body(&callbacks[0]), "noteless set");
    assert!(
        content.to_lowercase().contains("afk"),
        "noteless confirmation names the AFK state, got {content:?}"
    );
    runtime
        .on_message(&mention_message(1, MENTIONER_A, false, AFK_SETTER))
        .await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "noteless status still answers one mention");
    let reply = post_content(&posts[0]);
    assert!(
        reply.to_lowercase().contains("afk"),
        "noteless reply names the AFK state, got {reply:?}"
    );
    assert_no_repings(&post_body(&posts[0]), AFK_SETTER);
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_second_send_updates_stored_note() {
    let Some(db) = disposable_db("afk_second_send_updates_stored_note").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(703, AFK_SETTER, Some(NOTE)))
        .await;
    runtime
        .on_interaction(&afk_slash(704, AFK_SETTER, Some(UPDATED_NOTE)))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 2, "set and update each confirm once");
    let updated = assert_ephemeral_confirmation(&callback_body(&callbacks[1]), "update");
    assert!(
        updated.contains(UPDATED_NOTE),
        "update confirmation echoes the new note, got {updated:?}"
    );
    runtime
        .on_message(&mention_message(2, MENTIONER_A, false, AFK_SETTER))
        .await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "one mention auto-reply");
    let reply = post_content(&posts[0]);
    assert!(
        reply.contains(UPDATED_NOTE),
        "auto-reply carries the updated note, got {reply:?}"
    );
    assert!(
        !reply.contains(NOTE),
        "auto-reply drops the replaced note, got {reply:?}"
    );
    assert_no_repings(&post_body(&posts[0]), AFK_SETTER);
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_mention_yields_single_reply_with_note_elapsed_and_no_repings() {
    let Some(db) =
        disposable_db("afk_mention_yields_single_reply_with_note_elapsed_and_no_repings").await
    else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(705, AFK_SETTER, Some(NOTE)))
        .await;
    // Push elapsed past one second so sub-second renderings cannot apply.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    runtime
        .on_message(&mention_message(3, MENTIONER_A, false, AFK_SETTER))
        .await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "exactly one auto-reply per mention");
    assert!(
        posts[0]
            .path
            .ends_with(&format!("/channels/{CHANNEL}/messages")),
        "auto-reply posts to the mentioning channel, got {}",
        posts[0].path
    );
    let body = post_body(&posts[0]);
    let reply = body["content"].as_str().expect("auto-reply text");
    assert!(
        reply.contains(NOTE),
        "auto-reply echoes the note verbatim, got {reply:?}"
    );
    assert!(
        reply.to_lowercase().contains("afk"),
        "auto-reply names the AFK state, got {reply:?}"
    );
    assert!(
        contains_elapsed(reply),
        "auto-reply carries elapsed time, got {reply:?}"
    );
    assert_no_repings(&body, AFK_SETTER);
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_mention_rate_limited_per_mentioner() {
    let Some(db) = disposable_db("afk_mention_rate_limited_per_mentioner").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(706, AFK_SETTER, Some(NOTE)))
        .await;
    runtime
        .on_message(&mention_message(4, MENTIONER_A, false, AFK_SETTER))
        .await;
    runtime
        .on_message(&mention_message(5, MENTIONER_A, false, AFK_SETTER))
        .await;
    runtime
        .on_message(&mention_message(6, MENTIONER_B, false, AFK_SETTER))
        .await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(
        posts.len(),
        2,
        "an immediate repeat by the same member is suppressed while a different member still answers"
    );
    for post in &posts {
        let body = post_body(post);
        let reply = body["content"].as_str().expect("auto-reply text");
        assert!(
            reply.contains(NOTE),
            "every auto-reply echoes the note, got {reply:?}"
        );
        assert_no_repings(&body, AFK_SETTER);
    }
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_member_message_auto_clears_with_quiet_confirmation() {
    let Some(db) = disposable_db("afk_member_message_auto_clears_with_quiet_confirmation").await
    else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(707, AFK_SETTER, Some(NOTE)))
        .await;
    runtime.on_message(&plain_message(7, AFK_SETTER)).await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "auto-clear posts one quiet confirmation");
    let body = post_body(&posts[0]);
    let confirmation = body["content"].as_str().expect("confirmation text");
    assert!(
        confirmation.to_lowercase().contains("afk"),
        "confirmation names the cleared AFK state, got {confirmation:?}"
    );
    assert!(
        !confirmation.contains("<@")
            && !confirmation.contains("@everyone")
            && !confirmation.contains("@here"),
        "quiet confirmation pings nobody, got {confirmation:?}"
    );
    let parse = body["allowed_mentions"]["parse"].clone();
    assert!(
        parse.is_null() || parse == json!([]),
        "quiet confirmation parses no mentions, got {parse:?}"
    );
    runtime
        .on_message(&mention_message(8, MENTIONER_A, false, AFK_SETTER))
        .await;
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "cleared status answers no mentions"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_self_mention_clears_without_auto_reply() {
    let Some(db) = disposable_db("afk_self_mention_clears_without_auto_reply").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(708, AFK_SETTER, Some(NOTE)))
        .await;
    // A self-mention is still the member's own message: it clears with the
    // quiet confirmation and never triggers an auto-reply.
    runtime
        .on_message(&mention_message(9, AFK_SETTER, false, AFK_SETTER))
        .await;
    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "self-mention clears with one confirmation");
    let confirmation = post_content(&posts[0]);
    assert!(
        confirmation.to_lowercase().contains("afk"),
        "self-mention confirmation names the cleared state, got {confirmation:?}"
    );
    assert!(
        !confirmation.contains("<@"),
        "self-mention confirmation pings nobody, got {confirmation:?}"
    );
    runtime
        .on_message(&mention_message(10, MENTIONER_A, false, AFK_SETTER))
        .await;
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "self-mention cleared the status: later mentions stay silent"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_over_cap_note_refused_with_zero_state() {
    let Some(db) = disposable_db("afk_over_cap_note_refused_with_zero_state").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    let huge = "x".repeat(2000);
    runtime
        .on_interaction(&afk_slash(709, AFK_SETTER, Some(&huge)))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "over-cap note gets one refusal");
    let content = assert_ephemeral_confirmation(&callback_body(&callbacks[0]), "over-cap refusal");
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "afk is owned: refusal, not unknown-command"
    );
    assert!(
        content.to_lowercase().contains("too long"),
        "refusal names the length bound, got {content:?}"
    );
    assert!(
        !content.contains(huge.as_str()),
        "refusal never echoes the over-cap payload"
    );
    runtime
        .on_message(&mention_message(11, MENTIONER_A, false, AFK_SETTER))
        .await;
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "refused note stores zero state"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_hostile_note_refused_with_zero_state() {
    let Some(db) = disposable_db("afk_hostile_note_refused_with_zero_state").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    let cases = [
        (710, 77_u64, "ping @everyone at dawn"),
        (711, 78_u64, "alert @here now"),
        (712, 79_u64, "see <@99> for details"),
    ];
    for (interaction_id, setter, note) in cases {
        runtime
            .on_interaction(&afk_slash(interaction_id, setter, Some(note)))
            .await;
    }
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 3, "each hostile note gets one refusal");
    for (callback, (_, _, note)) in callbacks.iter().zip(cases) {
        let content = assert_ephemeral_confirmation(&callback_body(callback), "hostile refusal");
        assert_ne!(
            content, UNKNOWN_COMMAND_REPLY,
            "afk is owned: refusal, not unknown-command"
        );
        assert!(
            !content.contains("<@"),
            "refusal carries no mention syntax, got {content:?}"
        );
        assert!(
            !content.contains("@everyone") && !content.contains("@here"),
            "refusal carries no mass mention, got {content:?}"
        );
        assert!(
            !content.contains(note),
            "refusal never echoes the hostile payload"
        );
    }
    runtime
        .on_message(&mention_message(12, MENTIONER_A, false, 77))
        .await;
    runtime
        .on_message(&mention_message(13, MENTIONER_A, false, 78))
        .await;
    runtime
        .on_message(&mention_message(14, MENTIONER_A, false, 79))
        .await;
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "refused notes store zero state"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_bot_mention_never_triggers() {
    let Some(db) = disposable_db("afk_bot_mention_never_triggers").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(713, AFK_SETTER, Some(NOTE)))
        .await;
    runtime
        .on_message(&mention_message(15, 5555, true, AFK_SETTER))
        .await;
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "bot mentions never trigger an AFK reply"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}

#[tokio::test]
async fn afk_self_mention_by_other_member_never_triggers() {
    let Some(db) = disposable_db("afk_self_mention_by_other_member_never_triggers").await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(&db, origin).await;
    runtime
        .on_interaction(&afk_slash(714, AFK_SETTER, Some(NOTE)))
        .await;
    // The mentioner is not AFK, so no clear fires either: total silence.
    runtime
        .on_message(&mention_message(16, MENTIONER_A, false, MENTIONER_A))
        .await;
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "self-mentions never trigger an AFK reply"
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable test database");
}
