//! Protected feed-list acceptance through the real shard, decoder and runtime.
//! All Discord traffic is loopback; the parent fixture guards agent-testdb.
#![cfg(test)]

use super::*;

struct Observation {
    contents: Vec<String>,
    wire: Vec<(String, String, Value)>,
    before: Value,
    after: Value,
    foreign_requests: usize,
}

fn id(index: usize) -> String {
    format!("{index:032x}")
}

fn source(index: usize) -> String {
    format!("https://example.org/news/section-{index:02}/updates?format=rss")
}

async fn snapshot(pool: &PgPool) -> Value {
    // Snapshot every column and both guilds, not a projection that overlooks
    // enabled/check timestamps, delivery receipts, or foreign-guild writes.
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r),
            'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d),
            'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(pool)
    .await
    .expect("complete feed snapshot")
}

async fn seed_relays(pool: &PgPool, count: usize, hostile: bool) {
    // Reverse insertion and equal timestamps exercise the existing id tie-break.
    for index in (1..=count).rev() {
        sqlx::query(
            "INSERT INTO feed_relays (id, guild_id, channel_id, kind, source, enabled, created_by, created_at, updated_at)
             VALUES ($1, '2222', '123456789012345678', 'rss', $2, $3, '3333',
                     '2030-01-01T00:00:00Z', '2030-01-01T00:00:00Z')",
        )
        .bind(id(index))
        .bind(if hostile { format!("https://example.org/@everyone/section-{index}?format=rss") } else { source(index) })
        .bind(!index.is_multiple_of(5))
        .execute(pool)
        .await
        .expect("seed own relay");
    }
    sqlx::query(
        "INSERT INTO feed_relays (id, guild_id, channel_id, kind, source, created_by, created_at, updated_at)
         VALUES ('foreign-relay-secret', '9999', '7777', 'rss', 'https://foreign.example/private-feed', '3333',
                 '2030-01-01T00:00:00Z', '2030-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("seed foreign relay");
    for (feed_id, state) in [
        (id(1), "pending"),
        ("foreign-relay-secret".into(), "delivered"),
    ] {
        if count == 0 && feed_id == id(1) {
            continue;
        }
        sqlx::query(
            "INSERT INTO feed_deliveries (feed_id, item_key, nonce, state, message_id, first_seen_at, delivered_at)
             VALUES ($1, 'item-0', $2, $3, CASE WHEN $3 = 'delivered' THEN '9001' END,
                     '2030-01-01T00:00:00Z', CASE WHEN $3 = 'delivered' THEN timestamptz '2030-01-01T00:00:00Z' END)",
        )
        .bind(&feed_id)
        .bind(format!("nonce-{feed_id}"))
        .bind(state)
        .execute(pool)
        .await
        .expect("seed delivery receipt");
    }
}

fn packet(sequence: u64, page: Option<Value>) -> Value {
    let mut packet = slash(sequence, 6000 + sequence);
    packet["d"]["data"]["name"] = json!("feed-list");
    packet["d"]["member"]["permissions"] = json!("32");
    packet["d"]["data"]["options"] = page.map_or_else(|| json!([]), |value| json!([value]));
    packet
}

fn page_option(value: i64) -> Value {
    json!({"name": "page", "type": 4, "value": value})
}

fn indicator(content: &str) -> Option<(usize, usize)> {
    let rest = content.split("Page ").nth(1)?;
    let (current, rest) = rest.split_once(" of ")?;
    let total: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((current.trim().parse().ok()?, total.parse().ok()?))
}

async fn request_page(
    db: &TestDb,
    gateway: &CommandGateway,
    rest: &MockRest,
    invocation: usize,
    option: Option<Value>,
) -> String {
    let sequence = invocation as u64 + 1;
    gateway.send(packet(sequence, option)).await;
    wait_sequence(&db.store, sequence).await;
    wait_requests(rest, 3 + invocation * 2).await;
    let patches: Vec<_> = rest
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    let body: Value = serde_json::from_slice(&patches.last().expect("reply edit").body).unwrap();
    body["content"].as_str().expect("text reply").into()
}

async fn exercise(
    count: usize,
    options: Vec<Option<Value>>,
    walk: bool,
    hostile: bool,
) -> Observation {
    let db = TestDb::new().await;
    seed_relays(&db.pool, count, hostile).await;
    let before = snapshot(&db.pool).await;
    let mut script = bootstrap_responses();
    script.push(ScriptedResponse::json(200, json!([])));
    let rest = MockRest::start(script, ScriptedResponse::json(200, json!({"id": "9001"}))).await;
    let mut config = vars(Some("0"));
    config.insert("TWO_ANNOUNCEMENTS".into(), "1".into());
    let runtime = bootstrap(&db, &rest, &config).await;
    let mut gateway = CommandGateway::start(45000).await;
    let (runner, _) = spawn_runner_with_commands(&db, &gateway.url, Some(runtime.clone())).await;
    assert_eq!(gateway.authentication().await["op"], 2);
    gateway
        .send(ready(&gateway.url, "feed-pages-session"))
        .await;
    wait_sequence(&db.store, 1).await;
    wait_requests(&rest, 3).await;
    let mut contents = Vec::new();
    for option in options {
        contents.push(request_page(&db, &gateway, &rest, contents.len() + 1, option).await);
    }
    if walk {
        // Missing metadata is asserted after teardown. The fallback still
        // sends page 2 on RED, exposing a decoder that discards the option.
        let total = indicator(&contents[0]).map_or(2, |(_, total)| total);
        for page in 2..=total.min(count + 1) {
            contents.push(
                request_page(
                    &db,
                    &gateway,
                    &rest,
                    contents.len() + 1,
                    Some(page_option(page as i64)),
                )
                .await,
            );
        }
    }
    let wire = rest
        .requests()
        .into_iter()
        .skip(3)
        .map(|r| {
            (
                r.method,
                r.path,
                serde_json::from_slice(&r.body).expect("reply JSON"),
            )
        })
        .collect();
    // Await the shared runtime directly for the foreign-guild control, rather
    // than racing a detached gateway task that might not have run at assertion.
    let mut foreign = packet(900, Some(page_option(1)));
    foreign["d"]["guild_id"] = json!("9999");
    let interaction =
        serde_json::from_value(foreign["d"].clone()).expect("foreign wire interaction");
    let request_count = rest.requests().len();
    runtime.on_interaction(&interaction).await;
    let foreign_requests = rest.requests().len() - request_count;
    let after = snapshot(&db.pool).await;
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    rest.shutdown().await;
    drop(runtime);
    db.close().await;
    Observation {
        contents,
        wire,
        before,
        after,
        foreign_requests,
    }
}

fn assert_safe_read_only(observation: &Observation) {
    assert_eq!(
        observation.before, observation.after,
        "listing/refusals change no config, delivery or audit column in either guild"
    );
    assert_eq!(
        observation.foreign_requests, 0,
        "foreign-guild input stays ignored"
    );
    assert_eq!(
        observation.wire.len(),
        observation.contents.len() * 2,
        "no delivery, registration or extra reply REST calls"
    );
    for pair in observation.wire.as_chunks::<2>().0 {
        assert_eq!(pair[0].0, "POST");
        assert!(pair[0].1.ends_with("/callback"));
        assert_eq!(pair[0].2["type"], 5);
        assert_eq!(pair[0].2["data"]["flags"], 64, "ephemeral defer");
        assert_eq!(pair[1].0, "PATCH");
        assert!(pair[1].1.ends_with("/messages/@original"));
        assert_eq!(pair[1].2["allowed_mentions"]["parse"], json!([]));
    }
    for content in &observation.contents {
        assert!(
            content.encode_utf16().count() <= 2000,
            "navigation also counts toward the Discord limit"
        );
        assert!(!content.contains("foreign-relay-secret"));
        assert!(!content.contains("foreign.example"));
        assert!(!content.contains("@everyone"));
        assert!(!content.contains("@here"));
    }
}

fn assert_walk(observation: &Observation, count: usize) {
    assert_safe_read_only(observation);
    assert_eq!(
        observation.contents[0], observation.contents[1],
        "default page is explicitly page 1"
    );
    let (_, total) = indicator(&observation.contents[0])
        .expect("page/total metadata is required for a multi-page list");
    assert!((2..=count).contains(&total));
    let pages =
        std::iter::once(&observation.contents[0]).chain(observation.contents.iter().skip(2));
    let mut ids = Vec::new();
    for (index, content) in pages.enumerate() {
        assert_eq!(indicator(content), Some((index + 1, total)));
        assert!(
            content.contains("/feed-list") && content.contains("page"),
            "give concise usable navigation: {content}"
        );
        let rows: Vec<_> = content
            .lines()
            .filter(|line| line.starts_with('`'))
            .collect();
        assert!(!rows.is_empty(), "no empty reachable pages");
        for line in rows {
            let relay_id = line
                .strip_prefix('`')
                .unwrap()
                .split_once('`')
                .expect("whole ID")
                .0;
            let index = usize::from_str_radix(relay_id, 16).expect("exact fixture ID");
            assert_eq!(relay_id, id(index));
            assert!(line.contains("rss") && line.contains("<#123456789012345678>"));
            assert!(
                line.contains(&source(index)),
                "middle path source identity must survive capacity: {line}"
            );
            ids.push(relay_id.to_owned());
        }
    }
    assert_eq!(
        observation.contents.len(),
        total + 1,
        "walk every advertised page, plus explicit page-1 repeat"
    );
    assert_eq!(
        ids,
        (1..=count).map(id).collect::<Vec<_>>(),
        "every enabled/disabled ID appears exactly once in stable store order"
    );
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_pages_gateway_capacity_preserves_sources_and_reachable_ids() {
    let observation = exercise(25, vec![None, Some(page_option(1))], true, false).await;
    assert_walk(&observation, 25);
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_pages_gateway_restored_excess_has_no_silent_id_only_tail_loss() {
    let observation = exercise(70, vec![None, Some(page_option(1))], true, false).await;
    assert_walk(&observation, 70);
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_pages_gateway_invalid_pages_refuse_without_writes() {
    let observation = exercise(
        3,
        vec![
            Some(page_option(0)),
            Some(page_option(-1)),
            Some(page_option(2)),
            Some(page_option(i64::MAX)),
            Some(json!({"name": "page", "type": 3, "value": "second"})),
            Some(json!({"name": "page", "type": 10, "value": 1.5})),
        ],
        false,
        false,
    )
    .await;
    assert_safe_read_only(&observation);
    for content in &observation.contents {
        assert!(
            content.encode_utf16().count() <= 300,
            "invalid-page refusal is short: {content}"
        );
        assert!(
            content.to_ascii_lowercase().contains("page") && content.contains("/feed-list"),
            "refusal explains how to retry: {content}"
        );
        assert!(
            !content.lines().any(|line| line.starts_with('`')),
            "invalid input must not silently list page 1: {content}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_pages_gateway_empty_short_and_mentions_controls() {
    for (count, hostile) in [(0, false), (3, false), (3, true)] {
        let observation = exercise(count, vec![None, Some(page_option(1))], false, hostile).await;
        assert_safe_read_only(&observation);
        assert_eq!(observation.contents[0], observation.contents[1]);
        if count == 0 {
            assert_eq!(observation.contents[0], "No feed relays configured.");
        } else {
            for index in 1..=count {
                assert!(observation.contents[0].contains(&format!("`{}`", id(index))));
                if !hostile {
                    assert!(observation.contents[0].contains(&source(index)));
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_pages_gateway_empty_later_page_is_actionable_refusal() {
    let observation = exercise(0, vec![Some(page_option(2))], false, false).await;
    assert_safe_read_only(&observation);
    assert_ne!(observation.contents[0], "No feed relays configured.");
    assert!(observation.contents[0]
        .to_ascii_lowercase()
        .contains("page"));
    assert!(observation.contents[0].contains("/feed-list"));
}
