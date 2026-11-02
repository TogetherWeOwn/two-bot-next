//! Protected acceptance: pasted event references through real dispatch and stores.
//!
//! Contract: literal https://discord.com/events/<guild>/<event>, optionally
//! enclosed by one pair of angle brackets. IDs are nonzero, 17–20 ASCII digits
//! fitting u64. No alternate origin, URL decoding, query, fragment, trailing
//! slash, additional segment, userinfo or port. Guild must equal invocation.
//! Existing numeric IDs and numeric ID:label occurrences remain valid. URL
//! parsing is local; only the existing canonical Discord API reads may occur.

use super::*;
use common::RestRequest;
use two_bot_core::{put_rsvp, RsvpRecord, RsvpStatus};

const FOREIGN: &str = "1545644954272137001";
const SECOND_EVENT: &str = "1546451670500642001";
const THIRD: &str = "1546451670500642666";

fn link() -> String {
    format!("https://discord.com/events/{GUILD}/{EVENT}")
}

fn command(id: u64, name: &str, reference: &str) -> Interaction {
    match name {
        "rsvp" => slash(
            id,
            name,
            0,
            json!([
                {"name": "event-id", "type": 3, "value": reference},
                {"name": "status", "type": 3, "value": "going"}
            ]),
        ),
        "rsvp-attendance" => slash(
            id,
            name,
            0,
            json!([
                {"name": "event-id", "type": 3, "value": reference}
            ]),
        ),
        "attendance" => attendance(id, MANAGE_EVENTS, USER, reference),
        _ => panic!("unknown test command"),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    rsvps: String,
    audits: String,
    facts: String,
}

async fn state(pool: &Pool<Postgres>) -> State {
    let rsvps = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY guild_id,event_id,user_id), '[]'::jsonb)::text FROM (SELECT *, xmin::text AS version FROM event_rsvps) r")
        .fetch_one(pool).await.expect("RSVP snapshot");
    let audits = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb)::text FROM announcements_audit_log a")
        .fetch_one(pool).await.expect("audit/quota snapshot");
    let facts = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(f) ORDER BY id), '[]'::jsonb)::text FROM community_facts f")
        .fetch_one(pool).await.expect("attendance snapshot");
    State {
        rsvps,
        audits,
        facts,
    }
}

async fn seed(pool: &Pool<Postgres>, guild: &str, event_id: &str, user: &str, status: RsvpStatus) {
    put_rsvp(
        pool,
        &RsvpRecord {
            guild_id: guild.into(),
            event_id: event_id.into(),
            user_id: user.into(),
            status,
            responded_at: "2020-01-02T03:04:05.000Z".into(),
        },
    )
    .await
    .expect("seed historical response");
}

async fn live_rest() -> MockRest {
    MockRest::with_responder(|request| {
        if request.method == "POST" && request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
            ScriptedResponse::json(200, json!({"id": "99"}))
        } else if request.method == "GET" && request.path == scheduled_event_path(EVENT) {
            event(1)
        } else if request.method == "GET" && request.path == guild_member_path(USER) {
            member_get(USER, None)
        } else if request.method == "GET" && request.path == guild_member_path(OTHER) {
            member_get(OTHER, None)
        } else {
            ScriptedResponse::status(404)
        }
    })
    .await
}

/// Capture final wire text, not a private parser or a duplicated normalizer.
fn reply(requests: &[RestRequest]) -> String {
    let last = requests.last().expect("a private reply");
    let body: Value = serde_json::from_slice(&last.body).expect("reply JSON");
    if last.method == "PATCH" {
        assert!(last.path.ends_with("/messages/@original"));
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        body["content"].as_str().expect("edit content").into()
    } else {
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
        body["data"]["content"]
            .as_str()
            .expect("private refusal")
            .into()
    }
}

fn private_effects_only(requests: &[RestRequest], allow_live_reads: bool) {
    let first: Value = serde_json::from_slice(&requests[0].body).expect("callback");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    assert_eq!(first["data"]["flags"], 64, "never publish an RSVP/check-in");
    for request in requests {
        match request.method.as_str() {
            "GET" => {
                assert!(
                    allow_live_reads,
                    "parsing or historical reads must not fetch URLs"
                );
                assert!(
                    [
                        scheduled_event_path(EVENT),
                        guild_member_path(USER),
                        guild_member_path(OTHER),
                    ]
                    .contains(&request.path),
                    "noncanonical or extra lookup: {}",
                    request.path
                );
            }
            "POST" => assert!(request.path.ends_with("/callback"), "no channel/DM writes"),
            "PATCH" => assert_eq!(
                request.path,
                "/api/v10/webhooks/1111/mock-rsvp-token/messages/@original"
            ),
            _ => panic!(
                "unexpected Discord effect: {} {}",
                request.method, request.path
            ),
        }
    }
}

async fn rsvp_success(reference: &str, stable_confirmation: bool) {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = live_rest().await;
    run(fixture.pool(), &mock, &command(75000, "rsvp", reference)).await;
    let rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT guild_id,event_id,user_id,status FROM event_rsvps")
            .fetch_all(fixture.pool())
            .await
            .expect("saved identity");
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action,target_key FROM announcements_audit_log")
            .fetch_all(fixture.pool())
            .await
            .expect("saved audit");
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    let text = reply(&requests);
    assert_eq!(
        rows,
        vec![(GUILD.into(), EVENT.into(), USER.into(), "going".into())],
        "{text}"
    );
    assert_eq!(audits, vec![("event.rsvp".into(), EVENT.into())]);
    assert!(
        text.contains("RSVP saved:") && text.contains("going"),
        "{text}"
    );
    if stable_confirmation {
        assert!(
            text.contains(EVENT),
            "confirmation must identify the normalized event: {text}"
        );
        assert!(
            !text.contains("https://"),
            "do not echo a pasted URL as identity"
        );
    }
    private_effects_only(&requests, true);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == scheduled_event_path(EVENT))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == guild_member_path(USER))
            .count(),
        2
    );
}

#[tokio::test]
async fn raw_numeric_rsvp_control_keeps_live_reads_and_persisted_identity() {
    rsvp_success(EVENT, false).await;
}

#[tokio::test]
async fn canonical_link_rsvp_saves_the_existing_numeric_identity() {
    rsvp_success(&link(), true).await;
}

#[tokio::test]
async fn angle_wrapped_link_rsvp_saves_the_existing_numeric_identity() {
    rsvp_success(&format!("<{}>", link()), true).await;
}

async fn historical_totals(reference: &str) {
    let Some(fixture) = pool().await else {
        return;
    };
    seed(fixture.pool(), GUILD, EVENT, USER, RsvpStatus::Declined).await;
    seed(fixture.pool(), GUILD, EVENT, OTHER, RsvpStatus::Going).await;
    seed(fixture.pool(), FOREIGN, EVENT, USER, RsvpStatus::Interested).await;
    seed(
        fixture.pool(),
        GUILD,
        SECOND_EVENT,
        USER,
        RsvpStatus::Interested,
    )
    .await;
    let before = state(fixture.pool()).await;
    // All possible live reads would fail: the saved event may have been deleted.
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(404),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &command(75001, "rsvp-attendance", reference),
    )
    .await;
    let after = state(fixture.pool()).await;
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    assert_eq!(
        after, before,
        "reads cannot write, consume quota or refresh timestamps"
    );
    private_effects_only(&requests, false);
    assert_eq!(requests.len(), 2);
    let text = reply(&requests);
    assert!(text.contains(&format!("event `{EVENT}`")), "{text}");
    assert!(
        text.contains("Going: 1\nInterested: 0\nDeclined: 1"),
        "{text}"
    );
    assert!(text.contains("Your saved response: Declined"), "{text}");
    assert!(text.contains("<t:1577934245:f>"), "{text}");
    assert!(
        text.contains("live event availability is not checked"),
        "{text}"
    );
    for forbidden in [USER, OTHER, FOREIGN, SECOND_EVENT] {
        assert!(
            !text.contains(forbidden),
            "cross-member/guild/event disclosure: {text}"
        );
    }
}

#[tokio::test]
async fn raw_numeric_deleted_event_totals_control_remains_read_only() {
    historical_totals(EVENT).await;
}

#[tokio::test]
async fn canonical_link_deleted_event_totals_are_historical_not_a_live_lookup() {
    historical_totals(&link()).await;
}

#[tokio::test]
async fn angle_wrapped_deleted_event_totals_use_the_same_guild_event_snapshot() {
    historical_totals(&format!("<{}>", link())).await;
}

async fn checkin_success(reference: &str, target: &str, canonical: &str) {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = live_rest().await;
    let runtime = runtime(fixture.pool(), &mock);
    for (id, input) in [(75002, reference), (75003, canonical)] {
        assert!(runtime
            .handle(&attendance(id, MANAGE_EVENTS, target, input))
            .await
            .expect("dispatch"));
    }
    let count = counts(fixture.pool()).await;
    let facts: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT actor_id,source_event_id,source,idempotency_key,metadata FROM community_facts",
    )
    .fetch_all(fixture.pool())
    .await
    .expect("check-in facts");
    let audits: Vec<String> = sqlx::query_scalar("SELECT target_key FROM announcements_audit_log")
        .fetch_all(fixture.pool())
        .await
        .expect("check-in audit");
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    assert_eq!(
        count,
        (0, 1, 1),
        "link aliases must share the existing replay fence: {}",
        reply(&requests)
    );
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].0, target);
    assert_eq!(facts[0].1, format!("{canonical}:{target}"));
    assert_eq!(facts[0].2, format!("event:{canonical}"));
    assert_eq!(facts[0].3, format!("event-attended:{canonical}:{target}"));
    assert_eq!(
        serde_json::from_str::<Value>(&facts[0].4).expect("fact metadata"),
        json!({"eventOccurrenceId": canonical, "proof": "host_checkin"})
    );
    assert_eq!(audits, vec![format!("{canonical}:{target}")]);
    let text = reply(&requests);
    assert!(
        text.contains(canonical) && text.contains("already recorded"),
        "{text}"
    );
    assert!(
        !text.contains("https://"),
        "confirmation uses the canonical occurrence"
    );
    private_effects_only(&requests, true);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == scheduled_event_path(EVENT))
            .count(),
        3,
        "effective write has a post-write event fence; duplicate has none"
    );
}

#[tokio::test]
async fn numeric_and_labeled_checkin_controls_preserve_host_proof_and_deduplication() {
    checkin_success(EVENT, USER, EVENT).await;
    checkin_success(&anchored("weekly"), OTHER, &anchored("weekly")).await;
}

#[tokio::test]
async fn canonical_link_host_self_checkin_preserves_proof_and_deduplication() {
    checkin_success(&link(), USER, EVENT).await;
}

#[tokio::test]
async fn angle_wrapped_link_on_behalf_checkin_preserves_host_proof() {
    checkin_success(&format!("<{}>", link()), OTHER, EVENT).await;
}

fn invalid_references() -> Vec<String> {
    let url = link();
    vec![
        format!("https://discord.com/events/{FOREIGN}/{EVENT}"),
        url.replacen("https:", "http:", 1),
        url.replacen("https:", "ftp:", 1),
        url.replacen("discord.com", "discordapp.com", 1),
        url.replacen("discord.com", "www.discord.com", 1),
        url.replacen("discord.com", "discord.com.evil.example", 1),
        url.replacen("discord.com", "discord.com@evil.example", 1),
        url.replacen("discord.com", "user@discord.com", 1),
        url.replacen("discord.com", "discord.com:443", 1),
        url.replacen("discord.com", "discord.com.", 1),
        format!("{url}?event={SECOND_EVENT}"),
        format!("{url}?"),
        format!("{url}#other"),
        format!("{url}#"),
        format!("{url}/"),
        format!("{url}/extra"),
        format!("https://discord.com/events//{EVENT}"),
        format!("https://discord.com/events/{GUILD}/../{EVENT}"),
        format!("https://discord.com/events/{GUILD}/%31{EVENT}"),
        format!("https://discord.com/events/{GUILD}/{EVENT}%2fextra"),
        format!("https://discord.com/events/{GUILD}\\{EVENT}"),
        format!("https://discord.com/events/{GUILD}/18446744073709551616"),
        format!("https://discord.com/events/18446744073709551616/{EVENT}"),
        format!("https://discord.com/events/{GUILD}/00000000000000000"),
        format!("https://discord.com/events/{GUILD}/1234567890123456"),
        format!("https://discord.com/events/{GUILD}/１２３４５６７８９０１２３４５６７"),
        format!("<{url}"),
        format!("{url}>"),
        format!("<<{url}>>"),
        format!("[event]({url})"),
        format!("{url}\n{url}"),
        format!("{url}\0"),
        format!("{url}\u{200b}"),
        "18446744073709551616".into(),
        "99999999999999999999".into(),
        format!("{url}{}@everyone", "é".repeat(129)),
        format!("{url}{}@everyone", "🦀".repeat(129)),
    ]
}

async fn refusals(name: &str, values: Vec<String>) {
    let Some(fixture) = pool().await else {
        return;
    };
    let mut observed = Vec::new();
    for (i, value) in values.iter().enumerate() {
        let before = state(fixture.pool()).await;
        // A wrongly accepted reference must reveal a GET, not hide behind 404.
        let mock = live_rest().await;
        run(
            fixture.pool(),
            &mock,
            &command(76000 + i as u64, name, value),
        )
        .await;
        observed.push((
            value.clone(),
            before,
            state(fixture.pool()).await,
            mock.requests(),
        ));
        mock.shutdown().await;
    }
    fixture.close().await;
    for (value, before, after, requests) in observed {
        assert_eq!(
            after, before,
            "{name} changed rows for invalid reference {value:?}"
        );
        private_effects_only(&requests, false);
        let text = reply(&requests);
        assert!(
            !text.is_empty() && text.encode_utf16().count() <= 512,
            "bounded useful refusal: {text}"
        );
        assert!(
            !text.contains("RSVP saved:")
                && !text.contains("Recorded <@")
                && !text.contains("Stored RSVPs"),
            "not a refusal: {text}"
        );
        for sentinel in ["@everyone", "evil.example", "user@discord", "🦀", "\0"] {
            assert!(!text.contains(sentinel), "raw input echoed: {text}");
        }
    }
}

#[tokio::test]
async fn invalid_rsvp_reference_matrix_refuses_locally_without_writes() {
    refusals("rsvp", invalid_references()).await;
}

#[tokio::test]
async fn invalid_totals_reference_matrix_refuses_locally_without_writes() {
    refusals("rsvp-attendance", invalid_references()).await;
}

#[tokio::test]
async fn invalid_checkin_reference_matrix_refuses_locally_without_writes() {
    refusals("attendance", invalid_references()).await;
}

#[tokio::test]
async fn duplicate_and_wrong_type_options_refuse_before_any_live_lookup_or_write() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mut observations = Vec::new();
    for name in ["rsvp", "rsvp-attendance", "attendance"] {
        let key = if name == "attendance" {
            "event-occurrence"
        } else {
            "event-id"
        };
        let secondary = if name == "attendance" {
            "member"
        } else {
            "status"
        };
        let secondary_value = if name == "attendance" {
            json!({"name": secondary, "type": 6, "value": USER})
        } else {
            json!({"name": secondary, "type": 3, "value": "going"})
        };
        let mut cases = vec![
            json!([{ "name": key, "type": 3, "value": EVENT }, { "name": key, "type": 3, "value": EVENT }]),
            json!([{ "name": key, "type": 3, "value": EVENT }, { "name": key, "type": 3, "value": link() }]),
            json!([{ "name": key, "type": 4, "value": 42 }, { "name": key, "type": 3, "value": EVENT }]),
            json!([{ "name": key, "type": 3, "value": EVENT }, { "name": key, "type": 5, "value": true }]),
            json!([{ "name": key, "type": 4, "value": 42 }]),
        ];
        if name != "rsvp-attendance" {
            for options in &mut cases {
                options
                    .as_array_mut()
                    .expect("options")
                    .push(secondary_value.clone());
            }
            cases.push(json!([
                {"name": key, "type": 3, "value": EVENT}, secondary_value.clone(), secondary_value.clone()
            ]));
            cases.push(json!([
                {"name": key, "type": 3, "value": EVENT}, secondary_value.clone(),
                {"name": secondary, "type": 5, "value": true}
            ]));
        }
        for (i, options) in cases.into_iter().enumerate() {
            let before = state(fixture.pool()).await;
            let mock = live_rest().await;
            run(
                fixture.pool(),
                &mock,
                &slash(77000 + i as u64, name, MANAGE_EVENTS, options),
            )
            .await;
            observations.push((
                name,
                i,
                before,
                state(fixture.pool()).await,
                mock.requests(),
            ));
            mock.shutdown().await;
        }
    }
    fixture.close().await;
    for (name, i, before, after, requests) in observations {
        assert_eq!(after, before, "{name} malformed option case {i} wrote rows");
        private_effects_only(&requests, false);
        let text = reply(&requests);
        assert!(
            text.to_lowercase().contains("option"),
            "useful option refusal for {name}/{i}: {text}"
        );
        assert!(text.encode_utf16().count() <= 512);
    }
}

#[tokio::test]
async fn link_host_checkin_still_requires_manage_events_for_self_and_on_behalf() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = live_rest().await;
    for target in [USER, OTHER] {
        run(
            fixture.pool(),
            &mock,
            &attendance(78000, 0, target, &link()),
        )
        .await;
    }
    let count = counts(fixture.pool()).await;
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    assert_eq!(count, (0, 0, 0));
    assert_eq!(requests.len(), 2);
    assert!(reply(&requests).contains("Manage Events"));
    private_effects_only(&requests, false);
}

#[tokio::test]
async fn normalized_rsvp_still_checks_current_membership_and_live_event() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mut observed = Vec::new();
    for (responses, expected, get_count) in [
        (vec![ScriptedResponse::status(404)], "no longer a member", 1),
        (
            vec![member(USER), ScriptedResponse::status(404)],
            "No scheduled event",
            2,
        ),
        (vec![member(USER), event(4)], "cancelled", 2),
        (
            vec![
                member(USER),
                ScriptedResponse::json(200, json!({"id": EVENT, "guild_id": FOREIGN, "status": 1})),
            ],
            "invalid scheduled event",
            2,
        ),
    ] {
        let mut script = vec![ScriptedResponse::status(204)];
        script.extend(responses);
        script.push(ScriptedResponse::json(200, json!({"id": "99"})));
        let mock = MockRest::start(script, ScriptedResponse::status(404)).await;
        run(fixture.pool(), &mock, &command(78001, "rsvp", &link())).await;
        observed.push((
            counts(fixture.pool()).await,
            mock.requests(),
            expected,
            get_count,
        ));
        mock.shutdown().await;
    }
    fixture.close().await;
    for (count, requests, expected, gets) in observed {
        assert_eq!(count, (0, 0, 0));
        private_effects_only(&requests, true);
        assert_eq!(requests.iter().filter(|r| r.method == "GET").count(), gets);
        assert!(reply(&requests).contains(expected), "{}", reply(&requests));
    }
}

#[tokio::test]
async fn normalized_checkin_still_verifies_host_and_target_membership() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mut observed = Vec::new();
    for responses in [
        vec![event(1), ScriptedResponse::status(404)],
        vec![event(1), member(USER), ScriptedResponse::status(404)],
        vec![
            event(1),
            member(USER),
            ScriptedResponse::json(200, json!({"user": {"id": THIRD}, "roles": []})),
        ],
    ] {
        let mut script = vec![ScriptedResponse::status(204)];
        script.extend(responses);
        script.push(ScriptedResponse::json(200, json!({"id": "99"})));
        let mock = MockRest::start(script, ScriptedResponse::status(404)).await;
        run(
            fixture.pool(),
            &mock,
            &attendance(78002, MANAGE_EVENTS, OTHER, &link()),
        )
        .await;
        observed.push((counts(fixture.pool()).await, mock.requests()));
        mock.shutdown().await;
    }
    fixture.close().await;
    for (count, requests) in observed {
        assert_eq!(count, (0, 0, 0));
        private_effects_only(&requests, true);
        assert!(
            requests.iter().any(|r| r.path == guild_member_path(USER)),
            "host gate not reached"
        );
        assert!(reply(&requests).to_lowercase().contains("member"));
    }
}

#[tokio::test]
async fn normalized_rsvp_postwrite_loss_restores_exact_prior_response() {
    let Some(fixture) = pool().await else {
        return;
    };
    seed(fixture.pool(), GUILD, EVENT, USER, RsvpStatus::Declined).await;
    let before: (String, String) =
        sqlx::query_as("SELECT status,responded_at::text FROM event_rsvps")
            .fetch_one(fixture.pool())
            .await
            .expect("prior response");
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            member(USER),
            event(1),
            member(USER),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(404),
    )
    .await;
    run(fixture.pool(), &mock, &command(78003, "rsvp", &link())).await;
    let after: (String, String) =
        sqlx::query_as("SELECT status,responded_at::text FROM event_rsvps")
            .fetch_one(fixture.pool())
            .await
            .expect("restored response");
    let count = counts(fixture.pool()).await;
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    assert_eq!(after, before);
    assert_eq!(count, (1, 0, 0));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == scheduled_event_path(EVENT))
            .count(),
        2,
        "normalization must reach the existing post-write compensation gate"
    );
    assert!(reply(&requests).contains("No scheduled event"));
}

#[tokio::test]
async fn normalized_checkin_postwrite_loss_compensates_fact_and_audit() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            event(1),
            member_get(USER, None),
            ScriptedResponse::status(404),
            ScriptedResponse::json(200, json!({"id": "99"})),
        ],
        ScriptedResponse::status(404),
    )
    .await;
    run(
        fixture.pool(),
        &mock,
        &attendance(78004, MANAGE_EVENTS, USER, &link()),
    )
    .await;
    let count = counts(fixture.pool()).await;
    let requests = mock.requests();
    fixture.close().await;
    mock.shutdown().await;
    assert_eq!(count, (0, 0, 0));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == scheduled_event_path(EVENT))
            .count(),
        2
    );
    assert!(reply(&requests).contains("No scheduled event"));
}

#[test]
fn published_event_reference_options_advertise_links_with_bounded_input() {
    let defs = router().publish_set(&[]).expect("published registry");
    for (name, key) in [
        ("rsvp", "event-id"),
        ("rsvp-attendance", "event-id"),
        ("attendance", "event-occurrence"),
    ] {
        let command = defs
            .iter()
            .find(|d| d.name == name)
            .expect("existing command");
        let option = command
            .options
            .iter()
            .find(|o| o.name == key)
            .expect("existing option");
        let copy = option.description.to_lowercase();
        assert!(
            copy.contains("link") || copy.contains("url"),
            "{name} hides the pasted-link input: {copy}"
        );
        let cap = option.max_length.expect("publish a finite input cap");
        assert!(
            (70..=128).contains(&cap),
            "admit longest angle-wrapped link, keep existing finite occurrence cap"
        );
    }
}

#[test]
fn help_exposes_copyable_event_link_syntax_without_losing_historical_totals() {
    let defs = router().publish_set(&[]).expect("published registry");
    let help = two_bot_core::help_text(&defs);
    assert!(
        help.contains("https://discord.com/events/"),
        "copyable canonical event-link example is missing: {help}"
    );
    assert!(help.contains("/rsvp-attendance") && help.contains("saved response"));
    assert!(help.encode_utf16().count() <= 2000);
}
