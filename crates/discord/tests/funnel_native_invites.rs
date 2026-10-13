//! Discord-native funnel acceptance: permanent invites and vanity check.
//!
//! FAILING acceptance tests for the funnel build. The contract below names the
//! items the build adds; none of them exist yet, so this target stays red
//! until the build lands. The build must not edit this file: turn it green by
//! implementing the contract in library code only.
//!
//! Builder contract, all in library code:
//!
//! * New module `two_bot_discord::funnel_invites` with:
//!   * `pub struct PermanentInvite { pub code: String, pub channel_id: String }`
//!     (derive `Debug`, `Clone`, `PartialEq`, `Eq`).
//!   * `pub enum VanityStatus { Active { code: String }, Absent, CapabilityGated }`
//!     (derive `Debug`, `Clone`, `PartialEq`, `Eq`). `CapabilityGated` covers
//!     every Discord refusal that means "this guild cannot have a vanity URL"
//!     (forbidden, unknown endpoint): the check reports the gate, it never
//!     fails the funnel.
//!   * `pub enum InviteActionError` with at least a `NotPermanent` variant
//!     (derive `Debug`, `Clone`, `PartialEq`, `Eq`), plus whatever the
//!     implementation needs for transport and Discord failures.
//! * On `ActionExecutor`:
//!   * `pub async fn create_permanent_invite(
//!       &self, channel_id: &str) -> Result<PermanentInvite, InviteActionError>`:
//!     `POST /channels/{channel_id}/invites` with
//!     `{max_age: 0, max_uses: 0, unique: true}` (Discord requires a JSON
//!     body and defaults an empty one to a 24-hour expiry, so only an
//!     explicit permanent body yields a permanent invite); a reply with
//!     `max_age` 0 and `max_uses` 0 reports the code, any other expiry
//!     reports `NotPermanent`. (Review amendment on PR #936: the empty-body
//!     contract could never succeed against real Discord.)
//!   * `pub async fn check_vanity_url(
//!       &self, guild_id: &str) -> Result<VanityStatus, InviteActionError>`:
//!     `GET /guilds/{guild_id}/vanity-url`; a set code reports `Active`, a
//!     null code reports `Absent`, and a refusal reports `CapabilityGated`.
//!
//! Offline only: a small loopback double stands in for Discord, so no token,
//! network, or database is touched.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use two_bot_discord::funnel_invites::{InviteActionError, VanityStatus};
use two_bot_discord::ActionExecutor;

const GUILD: &str = "100000000000000001";
const CHANNEL: &str = "100000000000000003";

/// One scripted HTTP reply, served in arrival order.
struct Scripted {
    status: u16,
    body: Vec<u8>,
}

/// One observed HTTP request: method, path, and raw body.
#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Minimal loopback HTTP double: records requests, replays queued replies,
/// and answers anything past the queue with an empty 500.
struct Loopback {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    task: JoinHandle<()>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Loopback {
    async fn start(script: Vec<Scripted>) -> Self {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) => panic!("loopback must bind: {error}"),
        };
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(error) => panic!("loopback must report its address: {error}"),
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let seen = Arc::clone(&recorded);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let seen = Arc::clone(&seen);
                let queue = Arc::clone(&queue);
                tokio::spawn(async move {
                    serve_conn(stream, &seen, &queue).await;
                });
            }
        });
        Self {
            addr,
            recorded,
            task,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        match self.recorded.lock() {
            Ok(guard) => guard.clone(),
            Err(error) => panic!("loopback recording must stay readable: {error}"),
        }
    }
}

async fn read_exactly(stream: &mut TcpStream, buf: &mut Vec<u8>, len: usize) {
    while buf.len() < len {
        let mut chunk = vec![0u8; len - buf.len()];
        match stream.read(&mut chunk).await {
            Ok(0) => panic!("loopback peer closed mid-body"),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(error) => panic!("loopback must keep reading the body: {error}"),
        }
    }
}

async fn serve_conn(
    mut stream: TcpStream,
    recorded: &Mutex<Vec<Recorded>>,
    queue: &Mutex<VecDeque<Scripted>>,
) {
    let mut head = Vec::new();
    loop {
        head.clear();
        loop {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte).await {
                Ok(0) => return,
                Ok(_) => head.push(byte[0]),
                Err(_) => return,
            }
            if head.len() > 65_536 {
                panic!("loopback refuses an oversized request head");
            }
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = match String::from_utf8(head.clone()) {
            Ok(text) => text,
            Err(error) => panic!("loopback only speaks UTF-8 heads: {error}"),
        };
        let mut lines = text.lines();
        let request_line = match lines.next() {
            Some(line) => line,
            None => panic!("loopback needs a request line"),
        };
        let mut parts = request_line.split_whitespace();
        let method = match parts.next() {
            Some(method) => method.to_owned(),
            None => panic!("loopback needs a request method"),
        };
        let path = match parts.next() {
            Some(path) => path.to_owned(),
            None => panic!("loopback needs a request path"),
        };
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                let value = value.trim_start_matches(' ');
                match value.parse::<usize>() {
                    Ok(len) => content_length = len,
                    Err(error) => panic!("loopback needs a numeric length: {error}"),
                }
            }
        }
        let mut body = Vec::new();
        read_exactly(&mut stream, &mut body, content_length).await;
        match recorded.lock() {
            Ok(mut guard) => guard.push(Recorded { method, path, body }),
            Err(error) => panic!("loopback recording must stay writable: {error}"),
        }
        let reply = match queue.lock() {
            Ok(mut guard) => guard.pop_front(),
            Err(error) => panic!("loopback script must stay readable: {error}"),
        };
        let (status, body) = match reply {
            Some(script) => (script.status, script.body),
            None => (500, Vec::new()),
        };
        let reason = match status {
            200 => "OK",
            201 => "Created",
            403 => "Forbidden",
            _ => "Internal Server Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
            body.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        if stream.write_all(&body).await.is_err() {
            return;
        }
    }
}

fn executor_for(loopback: &Loopback) -> ActionExecutor {
    match ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(loopback.origin())) {
        Ok(executor) => executor,
        Err(error) => panic!("executor must build against the loopback: {error}"),
    }
}

fn json_script(status: u16, body: serde_json::Value) -> Scripted {
    Scripted {
        status,
        body: body.to_string().into_bytes(),
    }
}

#[tokio::test]
async fn create_permanent_invite_posts_a_permanent_body_and_reports_the_code() {
    let loopback = Loopback::start(vec![
        json_script(
            200,
            json!({"code": "perm-1", "max_age": 0, "max_uses": 0, "temporary": false}),
        ),
        json_script(
            200,
            json!({"code": "temp-1", "max_age": 3600, "max_uses": 0, "temporary": false}),
        ),
    ])
    .await;
    let executor = executor_for(&loopback);
    let invite = executor.create_permanent_invite(CHANNEL).await;
    let invite = match invite {
        Ok(invite) => invite,
        Err(error) => panic!("a permanent invite reply must report the code: {error:?}"),
    };
    assert_eq!(invite.code, "perm-1");
    assert_eq!(invite.channel_id, CHANNEL);
    match executor.create_permanent_invite(CHANNEL).await {
        Ok(_) => panic!("an expiring invite must not report as permanent"),
        Err(error) => assert_eq!(error, InviteActionError::NotPermanent),
    }
    let requests = loopback.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert!(
            request
                .path
                .ends_with(&format!("/channels/{CHANNEL}/invites")),
            "unexpected invite path: {}",
            request.path
        );
        let body: serde_json::Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => panic!("permanent invites post a JSON body"),
        };
        assert_eq!(
            body,
            serde_json::json!({"max_age": 0, "max_uses": 0, "unique": true}),
            "permanent invites request a never-expiring unique invite"
        );
    }
}

#[tokio::test]
async fn vanity_check_reports_capability_gating_without_failing() {
    let loopback = Loopback::start(vec![
        json_script(200, json!({"code": "village-green", "uses": 3})),
        json_script(200, json!({"code": null, "uses": 0})),
        json_script(403, json!({"message": "Missing Access", "code": 50001})),
    ])
    .await;
    let executor = executor_for(&loopback);
    match executor.check_vanity_url(GUILD).await {
        Ok(VanityStatus::Active { code }) => assert_eq!(code, "village-green"),
        Ok(other) => panic!("a set vanity code must report active: {other:?}"),
        Err(error) => panic!("a set vanity code must not fail: {error:?}"),
    }
    match executor.check_vanity_url(GUILD).await {
        Ok(VanityStatus::Absent) => {}
        Ok(other) => panic!("an unset vanity code must report absent: {other:?}"),
        Err(error) => panic!("an unset vanity code must not fail: {error:?}"),
    }
    match executor.check_vanity_url(GUILD).await {
        Ok(VanityStatus::CapabilityGated) => {}
        Ok(other) => panic!("a refused vanity read must report gating: {other:?}"),
        Err(error) => panic!("a refused vanity read must not fail: {error:?}"),
    }
    let requests = loopback.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert!(
            request
                .path
                .ends_with(&format!("/guilds/{GUILD}/vanity-url")),
            "unexpected vanity path: {}",
            request.path
        );
    }
}
