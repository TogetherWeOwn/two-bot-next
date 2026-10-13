//! Discord-native funnel pins: EXTERNAL event wire contract and mirror rejection.
//!
//! Green acceptance pins for behavior the funnel build stands on. An event
//! input placed at a written location renders the Discord EXTERNAL shape
//! (`entity_type` 3, null `channel_id`, `entity_metadata.location`); an
//! upsert without an event id creates over
//! `POST /guilds/{guild}/scheduled-events` with that body on the wire; and
//! one malformed poll row still rejects the whole mirror snapshot.
//!
//! Offline only: a small loopback double stands in for Discord, so no token,
//! network, or database is touched. This file uses only long-built APIs and
//! stays green; the still-missing funnel surface lives in
//! `funnel_native_invites.rs`, which stays red until the build lands.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use two_bot_core::internal_actions::{EventInput, EventPlace};
use two_bot_core::{
    normalize_events, EventStatus, RawScheduledEvent, ScheduledEvent, ScheduledEventMirror,
};
use two_bot_discord::{scheduled_event_body, ActionExecutor, EventCall};

const GUILD: &str = "100000000000000001";
const EVENT: &str = "100000000000000002";
const LOCATION: &str = "The community hall, game night";
const OBSERVED: &str = "2026-09-01T18:00:00.000Z";

fn external_input() -> EventInput {
    EventInput {
        name: "Game Night".to_owned(),
        starts_at: "2026-09-01T19:00:00.000Z".to_owned(),
        ends_at: "2026-09-01T22:00:00.000Z".to_owned(),
        description: None,
        place: EventPlace::Location(LOCATION.to_owned()),
    }
}

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
            if let Some(value) = line.strip_prefix("content-length:") {
                let value = value.trim_start_matches(' ');
                match value.parse::<usize>() {
                    Ok(len) => content_length = len,
                    Err(error) => panic!("loopback needs a numeric length: {error}"),
                }
            } else if let Some(value) = line.strip_prefix("Content-Length:") {
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

struct PinMirror {
    rows: Mutex<Vec<(String, ScheduledEvent, String)>>,
}

impl ScheduledEventMirror for PinMirror {
    async fn upsert(
        &self,
        guild_id: &str,
        observed_at: &str,
        event: &ScheduledEvent,
    ) -> Result<(), String> {
        match self.rows.lock() {
            Ok(mut guard) => {
                guard.push((guild_id.to_owned(), event.clone(), observed_at.to_owned()));
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn json_body(raw: &[u8]) -> Value {
    match serde_json::from_slice(raw) {
        Ok(body) => body,
        Err(error) => panic!("loopback must see JSON bodies: {error}"),
    }
}

#[test]
fn external_input_renders_the_external_wire_shape() {
    let body = scheduled_event_body(&external_input());
    assert_eq!(body.get("entity_type").and_then(Value::as_i64), Some(3));
    assert_eq!(body.get("channel_id"), Some(&Value::Null));
    assert_eq!(
        body.get("entity_metadata")
            .and_then(|metadata| metadata.get("location"))
            .and_then(Value::as_str),
        Some(LOCATION)
    );
    assert_eq!(body.get("name").and_then(Value::as_str), Some("Game Night"));
    assert!(body.get("description").is_none());
}

#[tokio::test]
async fn upsert_without_id_creates_an_external_event_on_the_wire() {
    let created = json!({
        "id": EVENT,
        "guild_id": GUILD,
        "name": "Game Night",
        "scheduled_start_time": "2026-09-01T19:00:00.000Z",
        "status": 1,
    });
    let loopback = Loopback::start(vec![Scripted {
        status: 201,
        body: created.to_string().into_bytes(),
    }])
    .await;
    let executor =
        match ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(loopback.origin())) {
            Ok(executor) => executor,
            Err(error) => panic!("executor must build against the loopback: {error}"),
        };
    let mirror = PinMirror {
        rows: Mutex::new(Vec::new()),
    };
    let outcome = executor
        .execute_event(
            GUILD,
            &EventCall::Upsert {
                event_id: None,
                input: external_input(),
            },
            &mirror,
            || OBSERVED.to_owned(),
        )
        .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => panic!("external create must succeed: {error}"),
    };
    assert_eq!(outcome, json!({"outcome": "created", "event_id": EVENT}));
    let requests = loopback.requests();
    assert_eq!(requests.len(), 1);
    let request = match requests.first() {
        Some(request) => request,
        None => panic!("loopback must record the create"),
    };
    assert_eq!(request.method, "POST");
    assert!(
        request
            .path
            .ends_with(&format!("/guilds/{GUILD}/scheduled-events")),
        "unexpected create path: {}",
        request.path
    );
    let sent = json_body(&request.body);
    assert_eq!(sent.get("entity_type").and_then(Value::as_i64), Some(3));
    assert_eq!(sent.get("channel_id"), Some(&Value::Null));
    assert_eq!(
        sent.get("entity_metadata")
            .and_then(|metadata| metadata.get("location"))
            .and_then(Value::as_str),
        Some(LOCATION)
    );
    let rows = match mirror.rows.lock() {
        Ok(guard) => guard.clone(),
        Err(error) => panic!("mirror must stay readable: {error}"),
    };
    assert_eq!(rows.len(), 1);
    let (guild_id, event, observed_at) = match rows.first() {
        Some(row) => row,
        None => panic!("create must refresh the mirror"),
    };
    assert_eq!(guild_id, GUILD);
    assert_eq!(event.id, EVENT);
    assert_eq!(event.channel_id, None);
    assert_eq!(event.status, EventStatus::Scheduled);
    assert_eq!(observed_at, OBSERVED);
}

#[test]
fn one_malformed_poll_row_rejects_the_snapshot() {
    let good = RawScheduledEvent {
        id: Some("event-1".to_owned()),
        name: Some("Game night".to_owned()),
        scheduled_start_time: Some("2026-09-06T18:00:00.000Z".to_owned()),
        channel_id: None,
        description: None,
        status: Some(1),
    };
    let unknown_status = RawScheduledEvent {
        status: Some(99),
        ..good.clone()
    };
    assert!(normalize_events(&[good.clone(), unknown_status]).is_none());
    match normalize_events(&[good]) {
        Some(rows) => assert_eq!(rows.len(), 1),
        None => panic!("a clean row must normalize"),
    }
    assert!(normalize_events(&[]).is_some());
}
