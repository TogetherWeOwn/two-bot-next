//! Synthetic loopback history pages only: no Discord or database access.
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use twilight_model::id::Id;
use two_bot_cutover::cli::ScanReport;
use two_bot_cutover::{RestClient, ScanCompletion};

struct MockHistory {
    origin: String,
    paths: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockHistory {
    async fn start(script: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorded = paths.clone();
        let task = tokio::spawn(async move {
            let mut script = script.into_iter();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 64 * 1024);
                }
                let head = String::from_utf8_lossy(&request);
                let path = head.split_whitespace().nth(1).unwrap().to_owned();
                recorded.lock().unwrap().push(path);
                let (status, body) = script.next().unwrap_or_else(|| failure(500));
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            origin,
            paths,
            task,
        }
    }

    fn client(&self) -> RestClient {
        RestClient::with_proxy("synthetic-test-token".to_owned(), Some(self.origin.clone()))
    }
}

impl Drop for MockHistory {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn failure(status: u16) -> (u16, String) {
    (
        status,
        json!({"code": 50001, "message": "synthetic failure"}).to_string(),
    )
}

fn messages(count: usize) -> (u16, String) {
    let batch: Vec<_> = (0..count).map(|i| json!({
        "id": (1000 - i).to_string(), "channel_id": "42",
        "author": {"id": "7", "username": "fixture", "discriminator": "0001", "avatar": null},
        "content": "fixture", "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
        "pinned": false, "type": 0
    })).collect();
    (200, serde_json::to_string(&batch).unwrap())
}

#[tokio::test]
async fn forbidden_after_full_page_preserves_partial_history_but_is_incomplete() {
    let mock = MockHistory::start(vec![messages(100), failure(403)]).await;
    let client = mock.client();
    let page = tokio::time::timeout(
        Duration::from_secs(30),
        client.scan_channel(Id::new(42), 10, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(page.messages.len(), 100);
    assert_eq!(page.messages.last().unwrap().id.get(), 901);
    assert!(page.scanned_back_to.is_some());
    assert_eq!(client.requests(), 2);
    let paths = mock.paths.lock().unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths[1].contains("before=901"));
    assert_eq!(page.completion, ScanCompletion::Unreadable);
    assert!(page.completion.interrupted());
    assert!(!page.truncated, "an interruption is not a page cap");
    let mut report = ScanReport::default();
    report.record("42", page.completion);
    let output = report.render();
    assert!(output.contains("unreadable=1"));
    assert!(output.contains("INCOMPLETE: unreadable on 42"));
    assert!(!output.contains("end-of-history="));
    assert!(!output.contains("page-cap="));
}

async fn scan(
    script: Vec<(u16, String)>,
    max_pages: usize,
    bound: Option<i64>,
) -> (two_bot_cutover::ScanPage, u64, Vec<String>) {
    let mock = MockHistory::start(script).await;
    let client = mock.client();
    let page = tokio::time::timeout(
        Duration::from_secs(30),
        client.scan_channel(Id::new(42), max_pages, bound),
    )
    .await
    .expect("history scan exceeded fixture deadline")
    .unwrap();
    let paths = mock.paths.lock().unwrap().clone();
    (page, client.requests(), paths)
}

#[tokio::test]
async fn exhausted_server_failures_are_not_end_of_history_and_keep_partial_rows() {
    let mut script = vec![messages(100)];
    script.extend((0..5).map(|_| failure(503)));
    let (page, requests, paths) = scan(script, 10, None).await;
    assert_eq!(page.messages.len(), 100);
    assert!(page.scanned_back_to.is_some());
    assert_eq!(page.completion, ScanCompletion::RetryExhausted);
    assert!(!page.truncated);
    assert_eq!(
        requests, 6,
        "one good page plus original attempt and four retries"
    );
    assert_eq!(paths.len(), 6);
    assert!(paths[1..].iter().all(|path| path == &paths[1]));
    let mut report = ScanReport::default();
    report.record("42", page.completion);
    assert!(report
        .render()
        .contains("INCOMPLETE: retry-exhausted on 42"));
}

#[tokio::test]
async fn failed_first_page_is_incomplete_even_with_no_rows() {
    for status in [403, 404] {
        let (page, requests, paths) = scan(vec![failure(status)], 10, None).await;
        assert!(page.messages.is_empty());
        assert_eq!(page.scanned_back_to, None);
        assert_eq!(page.completion, ScanCompletion::Unreadable);
        assert!(!page.truncated);
        assert_eq!(requests, 1);
        assert_eq!(paths.len(), 1);
    }
}

#[tokio::test]
async fn empty_and_short_pages_really_complete_history() {
    for count in [0, 3] {
        let (page, requests, _) = scan(vec![messages(count)], 10, None).await;
        assert_eq!(page.messages.len(), count);
        assert_eq!(page.completion, ScanCompletion::EndOfHistory);
        assert!(!page.truncated);
        assert_eq!(requests, 1);
        let mut report = ScanReport::default();
        report.record("42", page.completion);
        assert_eq!(
            report.render(),
            "  scan completion       end-of-history=1\n"
        );
        assert!(!report.has_incomplete_history());
    }
    let (page, requests, _) = scan(vec![messages(100), messages(0)], 10, None).await;
    assert_eq!(page.messages.len(), 100);
    assert_eq!(page.completion, ScanCompletion::EndOfHistory);
    assert_eq!(requests, 2);
}

#[tokio::test]
async fn page_cap_and_time_boundary_do_not_probe_an_extra_page() {
    let (page, requests, paths) = scan(vec![], 0, None).await;
    assert_eq!(page.completion, ScanCompletion::PageCap);
    assert!(page.truncated);
    assert!(page.messages.is_empty());
    assert_eq!(requests, 0);
    assert!(paths.is_empty());

    let (page, requests, _) = scan(vec![messages(100)], 1, None).await;
    assert_eq!(page.completion, ScanCompletion::PageCap);
    assert!(page.truncated);
    assert_eq!(page.messages.len(), 100);
    assert_eq!(requests, 1);

    let bound = two_bot_cutover::iso_to_millis("2026-01-02T00:00:00Z");
    let (page, requests, _) = scan(vec![messages(100)], 1, bound).await;
    assert_eq!(page.completion, ScanCompletion::TimeBoundary);
    assert!(!page.truncated);
    assert_eq!(page.messages.len(), 100);
    assert_eq!(requests, 1);

    // A short page takes precedence: we have reached all history, not just a bound.
    let (page, requests, _) = scan(vec![messages(3)], 1, bound).await;
    assert_eq!(page.completion, ScanCompletion::EndOfHistory);
    assert!(!page.truncated);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn a_recovered_retry_is_still_successful_and_cost_is_counted() {
    let (page, requests, paths) = scan(vec![failure(503), messages(3)], 10, None).await;
    assert_eq!(page.messages.len(), 3);
    assert_eq!(page.completion, ScanCompletion::EndOfHistory);
    assert!(!page.truncated);
    assert_eq!(requests, 2);
    assert_eq!(paths[0], paths[1]);
}

#[tokio::test]
async fn invalid_or_rejected_pages_keep_previously_read_history() {
    for (response, reason) in [
        (
            (200, "not json".to_owned()),
            ScanCompletion::InvalidResponse,
        ),
        (failure(400), ScanCompletion::RequestFailed),
    ] {
        let (page, requests, _) = scan(vec![messages(100), response], 10, None).await;
        assert_eq!(page.messages.len(), 100);
        assert_eq!(page.completion, reason);
        assert!(reason.interrupted());
        assert!(!page.truncated);
        assert_eq!(requests, 2);
    }
}

#[test]
fn shared_cli_summary_keeps_completion_reasons_separate() {
    let mut report = ScanReport::default();
    for (channel, reason) in [
        ("complete", ScanCompletion::EndOfHistory),
        ("bounded", ScanCompletion::TimeBoundary),
        ("capped", ScanCompletion::PageCap),
        ("unreadable", ScanCompletion::Unreadable),
        ("exhausted", ScanCompletion::RetryExhausted),
        ("failed", ScanCompletion::RequestFailed),
        ("invalid", ScanCompletion::InvalidResponse),
    ] {
        report.record(channel, reason);
    }
    let output = report.render();
    for label in [
        "end-of-history",
        "time-boundary",
        "page-cap",
        "unreadable",
        "retry-exhausted",
        "request-failed",
        "invalid-response",
    ] {
        assert!(output.contains(&format!("{label}=1")));
    }
    assert!(report.has_incomplete_history());
    assert_eq!(output.matches("INCOMPLETE:").count(), 4);
    assert!(!output.contains("INCOMPLETE: end-of-history"));
    assert!(!output.contains("INCOMPLETE: time-boundary"));
    assert!(!output.contains("INCOMPLETE: page-cap"));
    // A completed time window is still partial all-time history for AM7.
    for reason in [ScanCompletion::TimeBoundary, ScanCompletion::PageCap] {
        let mut bounded = ScanReport::default();
        bounded.record("42", reason);
        assert!(bounded.has_incomplete_history());
        assert!(!bounded.render().contains("INCOMPLETE: unreadable"));
    }
}
