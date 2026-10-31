//! `RestAutomodFacts` read-failure diagnostics: one throttled `WARN` per resolver window.
//!
//! Strict reads stay silent on success and 404 absence, return `None` on any
//! other failure, and emit one `automod_facts_read_failed` warning per
//! 60-second window per instance. The event carries only `message`, `step`
//! and `cause`: no raw error text, paths, snowflakes or response bodies. The
//! window is shared across steps and guilds, quiet reads neither emit nor move
//! it, and suppressed failures never extend it.

#[allow(dead_code)]
mod common;

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_core::automod_runtime::MessageSubject;
use two_bot_discord::automod_activation::{AutomodFacts, RestAutomodFacts};
use two_bot_discord::ratelimit_guard::{GuardConfig, RateLimitGuard};
use two_bot_discord::ActionExecutor;

const GUILD: &str = "177700000000000001";
const OTHER_GUILD: &str = "177700000000000009";
const CHANNEL: &str = "177700000000000002";
const MESSAGE: &str = "177700000000000003";
const AUTHOR: &str = "177700000000000004";
const BOT: &str = "177700000000000005";
const OWNER: &str = "177700000000000006";
const AUTHOR_ROLE: &str = "177700000000000007";
const BOT_ROLE: &str = "177700000000000008";
/// Response-body marker: the diagnostic must never repeat it.
const SENTINEL: &str = "facts-diagnostic-sentinel";

fn subject() -> MessageSubject {
    MessageSubject {
        guild_id: GUILD.to_owned(),
        channel_id: CHANNEL.to_owned(),
        message_id: MESSAGE.to_owned(),
        author_id: AUTHOR.to_owned(),
    }
}

fn facts_for(mock: &MockRest) -> RestAutomodFacts {
    let executor = ActionExecutor::with_proxy_and_guard(
        "automod-facts-diagnostic-token".to_owned(),
        Some(mock.origin()),
        Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("fixture guard")),
    )
    .expect("executor builds against the mock");
    RestAutomodFacts::new(executor, OWNER.to_owned(), HashSet::new())
}

fn guild_ok() -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({
            "id": GUILD, "owner_id": OWNER,
            "roles": [
                {"id": GUILD, "position": 0, "permissions": "0"},
                {"id": AUTHOR_ROLE, "position": 1, "permissions": "0"},
                {"id": BOT_ROLE, "position": 5, "permissions": "0"}
            ]
        }),
    )
}

fn member_ok(user: &str, role: &str) -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": [role]}))
}

fn bot_ok() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": BOT, "username": "bot", "bot": true}))
}

fn message_ok() -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({
            "id": MESSAGE, "channel_id": CHANNEL, "guild_id": GUILD, "type": 0,
            "content": "diagnostic fixture",
            "author": {"id": AUTHOR, "username": "mock", "discriminator": "0", "avatar": null, "bot": false},
            "timestamp": "2026-10-19T00:00:00.000000+00:00", "edited_timestamp": null,
            "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "tts": false
        }),
    )
}

/// Thrown status errors ignore the body, so the sentinel there only exercises
/// the no-leak assertion.
fn refused(status: u16) -> ScriptedResponse {
    ScriptedResponse::json(status, json!({"sentinel": SENTINEL}))
}

fn malformed() -> ScriptedResponse {
    let mut response = ScriptedResponse::status(200);
    response.body = format!("{SENTINEL}-not-json").into_bytes();
    response
}

/// Every strict `get_json_strict` facts-read site: the expected `step`, the
/// successful-read prefix before the injected failure, and the wire request
/// count (strict reads never retry). Bot identity acquisition
/// (`current_bot_user_id` via `call_once_raw_paced`, not `get_json_strict`)
/// is unchanged and outside this suite.
#[derive(Clone, Copy, Debug)]
enum Site {
    Guild,
    BotMember,
    TargetMember,
    Message,
    AuthorMember,
}

impl Site {
    const ALL: [Site; 5] = [
        Site::Guild,
        Site::BotMember,
        Site::TargetMember,
        Site::Message,
        Site::AuthorMember,
    ];

    fn step(self) -> &'static str {
        match self {
            Site::Guild => "guild",
            Site::BotMember => "bot_member",
            Site::TargetMember | Site::AuthorMember => "target_member",
            Site::Message => "message",
        }
    }

    fn script(self, failure: ScriptedResponse) -> Vec<ScriptedResponse> {
        match self {
            Site::Guild => vec![failure],
            Site::BotMember => vec![guild_ok(), bot_ok(), failure],
            Site::TargetMember => vec![guild_ok(), bot_ok(), member_ok(BOT, BOT_ROLE), failure],
            Site::Message => vec![failure],
            Site::AuthorMember => vec![message_ok(), failure],
        }
    }

    fn requests(self) -> usize {
        match self {
            Site::Guild | Site::Message => 1,
            Site::BotMember => 3,
            Site::TargetMember => 4,
            Site::AuthorMember => 2,
        }
    }

    async fn read(self, facts: &RestAutomodFacts) {
        let none = match self {
            Site::Guild | Site::BotMember | Site::TargetMember => {
                facts.target_facts(&subject()).await.is_none()
            }
            Site::Message | Site::AuthorMember => {
                facts.fetch_message(GUILD, CHANNEL, MESSAGE).await.is_none()
            }
        };
        assert!(none, "a failed {self:?} read returns None");
    }
}

/// Process-wide JSON log sink (precedent: `rsvp_runtime.rs`): callsite
/// interest is cached against the first registering thread, so a thread-local
/// default can miss events. Readers only consume bytes after their own mark.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_sink() -> LogSink {
    static SINK: OnceLock<LogSink> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = LogSink::default();
        let writer = sink.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install the process-wide log sink once");
        sink
    })
    .clone()
}

/// Serializes every test that reads a diagnostic window, so a failure injected
/// by one test can never appear inside another test's window.
static DIAGNOSTIC_WINDOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn log_mark() -> usize {
    log_sink().0.lock().unwrap().len()
}

fn diagnostics_since(mark: usize) -> Vec<Value> {
    let bytes = log_sink().0.lock().unwrap()[mark..].to_vec();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"]["message"] == "automod_facts_read_failed")
        .collect()
}

/// Exactly one warning with the fixed vocabulary (`message`, `step`,
/// `cause`) and none of the sentinel, snowflake IDs or request paths.
fn assert_single_diagnostic(events: Vec<Value>, step: &str, cause: &str) {
    let [diagnostic] = events.as_slice() else {
        panic!("expected one automod_facts_read_failed diagnostic, found {events:?}");
    };
    assert_eq!(diagnostic["level"], "WARN", "{diagnostic}");
    assert_eq!(
        diagnostic["fields"].as_object().map(|fields| fields.len()),
        Some(3),
        "{diagnostic}"
    );
    assert_eq!(diagnostic["fields"]["step"], step, "{diagnostic}");
    assert_eq!(diagnostic["fields"]["cause"], cause, "{diagnostic}");
    let shown = diagnostic.to_string();
    for (kind, value) in [
        ("sentinel", SENTINEL),
        ("guild id", GUILD),
        ("other guild id", OTHER_GUILD),
        ("channel id", CHANNEL),
        ("message id", MESSAGE),
        ("author id", AUTHOR),
        ("bot id", BOT),
        ("owner id", OWNER),
        ("request path", "/api/v10"),
    ] {
        assert!(!shown.contains(value), "diagnostic leaked a {kind}");
    }
}

/// Pause only between calls: a paused clock auto-advances during socket I/O,
/// so the window moves here and the clock runs for the loopback HTTP reads.
async fn advance_window(secs: u64) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(secs)).await;
    tokio::time::resume();
}

/// Every strict `get_json_strict` facts-read site maps its error to the fixed
/// cause vocabulary (403 is rejected, 429 is rate-limited, 503 and malformed
/// JSON are unavailable) while 404 absence stays silent. Each failure returns
/// `None` on exactly its chain of requests. Bot identity acquisition is a
/// separate read path and stays outside this suite.
#[tokio::test]
async fn failed_reads_warn_once_with_fixed_step_and_cause() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    // `None` cause means the read stays silent.
    let cases: [(u16, Option<&str>); 5] = [
        (403, Some("rejected")),
        (429, Some("rate_limited")),
        (503, Some("unavailable")),
        (200, Some("unavailable")),
        (404, None),
    ];
    for site in Site::ALL {
        for (status, cause) in cases {
            let failure = match status {
                200 => malformed(),
                404 => ScriptedResponse::status(404),
                _ => refused(status),
            };
            let mock = MockRest::start(site.script(failure), ScriptedResponse::status(500)).await;
            let facts = facts_for(&mock);
            let mark = log_mark();
            site.read(&facts).await;
            match cause {
                Some(cause) => {
                    assert_single_diagnostic(diagnostics_since(mark), site.step(), cause)
                }
                None => assert!(
                    diagnostics_since(mark).is_empty(),
                    "{site:?} absence must stay silent"
                ),
            }
            assert_eq!(
                mock.requests().len(),
                site.requests(),
                "{site:?} must not retry a strict read"
            );
            mock.shutdown().await;
        }
    }
}

/// Successful chains stay silent and resolve: exactly their six reads, no
/// diagnostic, `Some` values.
#[tokio::test]
async fn successful_reads_are_silent() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let mock = MockRest::start(
        vec![
            guild_ok(),
            bot_ok(),
            member_ok(BOT, BOT_ROLE),
            member_ok(AUTHOR, AUTHOR_ROLE),
            message_ok(),
            member_ok(AUTHOR, AUTHOR_ROLE),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let facts = facts_for(&mock);
    let mark = log_mark();
    assert!(
        facts.target_facts(&subject()).await.is_some(),
        "a complete chain resolves"
    );
    assert!(
        facts.fetch_message(GUILD, CHANNEL, MESSAGE).await.is_some(),
        "a complete message read resolves"
    );
    assert!(
        diagnostics_since(mark).is_empty(),
        "successful reads must stay silent"
    );
    assert_eq!(mock.requests().len(), 6);
    mock.shutdown().await;
}

/// One warning per 60 seconds per instance, shared across steps and guilds:
/// immediate repeats stay silent, the window still holds at 59 seconds and
/// reopens past 60 with margin for loopback I/O time.
#[tokio::test]
async fn repeats_share_one_sixty_second_window() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let mock = MockRest::start(vec![], refused(403)).await;
    let facts = facts_for(&mock);

    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");

    // Immediate repeats: same site, another step, another guild. The message
    // read fails at its own step (not guild) against this all-403 mock.
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert!(facts.fetch_message(GUILD, CHANNEL, MESSAGE).await.is_none());
    let mut other = subject();
    other.guild_id = OTHER_GUILD.to_owned();
    assert!(facts.target_facts(&other).await.is_none());
    assert!(
        diagnostics_since(mark).is_empty(),
        "repeats share the open window"
    );

    advance_window(59).await;
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert!(
        diagnostics_since(mark).is_empty(),
        "the window holds inside 60 seconds"
    );

    // 61 seconds total: past the window with margin for loopback I/O time.
    advance_window(2).await;
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");

    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert!(
        diagnostics_since(mark).is_empty(),
        "the reopened window suppresses repeats"
    );
    mock.shutdown().await;
}

/// Quiet reads neither emit nor consume the first window: successes and 404s
/// on a pristine instance stay silent, the next failure still emits at once,
/// and the window then behaves normally.
#[tokio::test]
async fn quiet_reads_neither_emit_nor_consume_the_window() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let mock = MockRest::start(
        vec![
            guild_ok(),
            bot_ok(),
            member_ok(BOT, BOT_ROLE),
            member_ok(AUTHOR, AUTHOR_ROLE),
            message_ok(),
            member_ok(AUTHOR, AUTHOR_ROLE),
            ScriptedResponse::status(404),
            ScriptedResponse::status(404),
            refused(403),
            refused(403),
            refused(403),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let facts = facts_for(&mock);

    let mark = log_mark();
    assert!(facts.target_facts(&subject()).await.is_some());
    assert!(facts.fetch_message(GUILD, CHANNEL, MESSAGE).await.is_some());
    Site::Guild.read(&facts).await;
    Site::Message.read(&facts).await;
    assert!(
        diagnostics_since(mark).is_empty(),
        "quiet reads must stay silent"
    );

    // The first failure still emits at once: quiet reads consumed no window.
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");

    // The opened window then suppresses and reopens normally.
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert!(
        diagnostics_since(mark).is_empty(),
        "the opened window suppresses"
    );

    advance_window(61).await;
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");
    assert_eq!(
        mock.requests().len(),
        11,
        "every chain reaches the wire exactly once"
    );
    mock.shutdown().await;
}

/// A suppressed failure never extends the window: a failure 59 seconds in
/// does not push reopening past 60 seconds from the first emission (61 with
/// margin for loopback I/O time).
#[tokio::test]
async fn suppressed_failures_do_not_extend_the_window() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let mock = MockRest::start(vec![], refused(403)).await;
    let facts = facts_for(&mock);

    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");

    advance_window(59).await;
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert!(
        diagnostics_since(mark).is_empty(),
        "the mid-window failure is suppressed"
    );

    advance_window(2).await;
    let mark = log_mark();
    facts.target_facts(&subject()).await;
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");
    mock.shutdown().await;
}

/// Concurrent failures on a fresh instance emit exactly once: every caller
/// still sees `None` and every chain still reaches the wire.
#[tokio::test]
async fn concurrent_failures_emit_once() {
    let _window = DIAGNOSTIC_WINDOW.lock().await;
    let mock = MockRest::start(vec![refused(403); 8], refused(403)).await;
    let facts = Arc::new(facts_for(&mock));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));

    let mark = log_mark();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (facts, barrier) = (Arc::clone(&facts), Arc::clone(&barrier));
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            facts.target_facts(&subject()).await.is_none()
        }));
    }
    for task in tasks {
        assert!(task.await.expect("read task"), "every caller sees None");
    }
    assert_single_diagnostic(diagnostics_since(mark), "guild", "rejected");
    assert_eq!(
        mock.requests().len(),
        8,
        "every chain reaches the wire exactly once"
    );
    mock.shutdown().await;
}
