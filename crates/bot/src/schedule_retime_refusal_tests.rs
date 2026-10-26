//! Guard failures must leave the real schedule and delivery ledger untouched.

#![cfg(test)]

use super::*;

async fn shape(pool: &PgPool, id: &str, fields: &str) {
    // `fields` is always a test-authored literal fragment in this file,
    // never interaction input; the id stays a bound parameter.
    let query = format!("UPDATE scheduled_messages SET {fields} WHERE id = $1");
    sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(id)
        .execute(pool)
        .await
        .expect("shape disposable fixture");
}

fn correction(text: &str, option_name: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains(option_name),
        "identify the invalid option: {text}"
    );
    assert!(!saved(text));
    assert!(
        !lower.contains("try again"),
        "invalid input is not a transient store failure: {text}"
    );
    assert!(
        [
            "invalid",
            "duplicate",
            "missing",
            "required",
            "empty",
            "long",
            "64",
            "128",
            "rfc3339",
            "timestamp",
            "offset",
            "future",
            "past",
            "range",
            "365",
            "525600"
        ]
        .iter()
        .any(|word| lower.contains(word)),
        "actionable input guidance: {text}"
    );
}

#[tokio::test]
async fn unknown_foreign_and_other_channel_ids_share_one_safe_miss_with_no_writes() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    let id = "same-miss-target";
    let mut observations = Vec::new();
    for fixture in ["unknown", "foreign", "other-channel"] {
        if fixture == "foreign" {
            seed(db.pool(), id, old).await;
            shape(db.pool(), id, "guild_id = '9999'").await;
        } else if fixture == "other-channel" {
            shape(db.pool(), id, "guild_id = '2222', channel_id = '4444'").await;
        }
        let before = state(db.pool()).await;
        let requests = invoke(db.pool(), retime(id, &iso(new, 0))).await;
        let after = state(db.pool()).await;
        observations.push((fixture, before, after, requests));
    }
    shape(db.pool(), id, "channel_id = '3333'").await;
    let control = invoke(db.pool(), retime(id, &iso(new, 0))).await;
    let final_rows = schedules(db.pool()).await;
    db.close().await.expect("drop disposable database");

    let mut receipts = Vec::new();
    for (fixture, before, after, requests) in observations {
        assert_eq!(after, before, "{fixture} miss changes no schedule or audit");
        let text = private_text(&requests);
        assert!(!saved(&text), "miss cannot claim a saved mutation");
        assert!(
            !text.contains("9999") && !text.contains("4444"),
            "no foreign scope details"
        );
        assert!(
            text.to_lowercase().contains("schedule-list")
                || text.to_lowercase().contains("not found")
                || text.to_lowercase().contains("no match"),
            "safe actionable miss: {text}"
        );
        receipts.push(text);
    }
    assert_eq!(
        receipts[0], receipts[1],
        "unknown and foreign are indistinguishable"
    );
    assert_eq!(
        receipts[0], receipts[2],
        "another channel is indistinguishable from a missing ID"
    );
    assert_eq!(final_rows[0]["id"], id);
    assert_eq!(
        final_rows[0]["next_run_at"],
        canonical(new),
        "same target is mutable only in its own guild/channel"
    );
    assert!(saved(&private_text(&control)));
}

#[tokio::test]
async fn authority_feature_guild_and_channel_gates_precede_all_store_effects() {
    let db = database().await;
    let old = future();
    let at = iso(old + 3_600_000, 0);
    seed(db.pool(), "gate-target", old).await;
    let mut cases = Vec::new();
    cases.push((
        "authority",
        from_member(retime("gate-target", &at), 88, Permissions::empty()),
        true,
        false,
    ));
    cases.push(("feature", retime("gate-target", &at), false, false));
    let mut dm = retime("gate-target", &at);
    set_guild(&mut dm, None);
    cases.push(("dm", dm, true, false));
    cases.push((
        "channel",
        slash(
            "schedule-retime",
            None,
            vec![
                option("id", CommandOptionValue::String("gate-target".to_owned())),
                option("at", CommandOptionValue::String(at.clone())),
            ],
        ),
        true,
        false,
    ));
    let mut foreign = retime("gate-target", &at);
    set_guild(&mut foreign, Some(Id::new(9999)));
    cases.push(("guild", foreign, true, true));
    let before = state(db.pool()).await;
    let mut observations = Vec::new();
    for (label, interaction, enabled, silent_allowed) in cases {
        let requests = invoke_gated(db.pool(), interaction, enabled).await;
        observations.push((label, state(db.pool()).await, requests, silent_allowed));
    }
    db.close().await.expect("drop disposable database");

    for (label, after, requests, silent_allowed) in observations {
        assert_eq!(
            after, before,
            "{label} refusal precedes every durable effect"
        );
        if silent_allowed && requests.is_empty() {
            continue;
        }
        let text = private_text(&requests);
        assert!(!saved(&text));
        assert!(
            [
                "manage",
                "permission",
                "disabled",
                "enabled",
                "channel",
                "guild",
                "server",
                "member"
            ]
            .iter()
            .any(|word| text.to_lowercase().contains(word)),
            "actionable {label} refusal: {text}"
        );
    }
}

fn invalid_options(id: &str, at: &str) -> Vec<(&'static str, Vec<CommandDataOption>)> {
    let string = |s: &str| CommandOptionValue::String(s.to_owned());
    let mut cases = vec![
        ("id", vec![option("at", string(at))]),
        ("at", vec![option("id", string(id))]),
    ];
    for bad in [
        string(""),
        string(&"x".repeat(129)),
        string(&"😀".repeat(65)),
        string(&format!("{}@everyone", "x".repeat(129))),
        CommandOptionValue::Integer(1),
        CommandOptionValue::Boolean(true),
        CommandOptionValue::SubCommand(vec![]),
    ] {
        cases.push(("id", vec![option("id", bad), option("at", string(at))]));
    }
    let now = u64::try_from(now_millis_for_test()).expect("positive clock");
    let second = iso(future(), 0)
        .split('.')
        .next()
        .expect("UTC seconds")
        .to_owned();
    let long_fraction = format!("{second}.123{}Z", "0".repeat(41));
    assert_eq!(
        long_fraction.len(),
        65,
        "parseable oversized timestamp fixture"
    );
    for bad in [
        string(""),
        string(&iso(now, 0)),
        string(&iso(now - 1, 0)),
        string("2026-02-30T12:00:00Z"),
        string("2027-01-01T12:00:00"),
        string("2027-01-01T12:00:00+25:00"),
        string("99999999999999999999-01-01T00:00:00Z"),
        string(&iso(now + 367 * 86_400_000, 0)),
        string(&long_fraction),
        string(&"😀".repeat(33)),
        string(&"@everyone".repeat(10)),
        CommandOptionValue::Integer(30),
        CommandOptionValue::Boolean(false),
        CommandOptionValue::SubCommand(vec![]),
    ] {
        cases.push(("at", vec![option("id", string(id)), option("at", bad)]));
    }
    for (name, valid, other) in [("id", id, "different-id"), ("at", at, "not-a-time")] {
        for pair in [
            [string(valid), string(valid)],
            [string(valid), string(other)],
            [string(other), string(valid)],
            [string(valid), CommandOptionValue::Integer(7)],
            [CommandOptionValue::Integer(7), string(valid)],
        ] {
            let mut options = if name == "id" {
                vec![option("at", string(at))]
            } else {
                vec![option("id", string(id))]
            };
            options.extend(pair.into_iter().map(|value| option(name, value)));
            cases.push((name, options));
        }
    }
    // A preview-shaped extra cannot rescue an invalid timestamp or cause a
    // public effect. This does not require adding a preview feature.
    cases.push((
        "at",
        vec![
            option("id", string(id)),
            option("at", string("not-a-time")),
            option("preview", CommandOptionValue::Boolean(true)),
        ],
    ));
    cases
}

#[tokio::test]
async fn strict_option_and_time_refusals_leave_all_durable_state_unchanged() {
    let db = database().await;
    let old = future();
    seed(db.pool(), "decode-target", old).await;
    let before = state(db.pool()).await;
    let mut observations = Vec::new();
    for (name, options) in invalid_options("decode-target", &iso(old + 3_600_000, 0)) {
        let requests = invoke(db.pool(), slash("schedule-retime", Some(CHANNEL), options)).await;
        observations.push((name, requests, state(db.pool()).await));
    }
    db.close().await.expect("drop disposable database");
    for (name, requests, after) in observations {
        assert_eq!(
            after, before,
            "invalid {name} changes no schedule, audit or fence"
        );
        correction(&private_text(&requests), name);
    }
}

#[tokio::test]
async fn ineligible_states_preserve_all_claim_nonce_uncertainty_and_run_evidence() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    let active_claim = format!(
        "claim_token = 'held-claim', claimed_at = '{}', next_run_at = '{}', occurrence_nonce = 'held-nonce'",
        canonical(old), canonical(old + 60_000),
    );
    let cases = [
        ("recurring", "interval_seconds = 3600"),
        ("disabled", "enabled = FALSE"),
        (
            "completed",
            "enabled = FALSE, last_run_at = '2020-01-01T00:00:00.000Z', last_message_id = '9001'",
        ),
        ("previously-run", "last_run_at = '2020-01-01T00:00:00.000Z'"),
        ("message-evidence", "last_message_id = '9002'"),
        ("claimed", active_claim.as_str()),
        (
            "uncertain",
            "delivery_uncertain = TRUE, occurrence_nonce = 'uncertain-nonce'",
        ),
        (
            "unreadable-fence",
            "delivery_uncertain = NULL, occurrence_nonce = 'unreadable-nonce'",
        ),
    ];
    let mut observations = Vec::new();
    for (id, fields) in cases {
        seed(db.pool(), id, old).await;
        shape(db.pool(), id, fields).await;
        let before = schedules(db.pool()).await;
        let audits_before = state(db.pool()).await;
        let requests = invoke(db.pool(), retime(id, &iso(new, 0))).await;
        observations.push((
            id,
            before,
            schedules(db.pool()).await,
            audits_before,
            state(db.pool()).await,
            requests,
        ));
    }
    db.close().await.expect("drop disposable database");

    for (id, before, after, audits_before, audits_after, requests) in observations {
        assert_eq!(
            after, before,
            "{id} refuses without resetting ANY schedule evidence"
        );
        let successes = |state: &Value| {
            state["audits"]
                .as_array()
                .expect("audits")
                .iter()
                .filter(|audit| audit["outcome"] == "ok")
                .count()
        };
        assert_eq!(
            successes(&audits_after),
            successes(&audits_before),
            "no successful mutation audit for {id}"
        );
        let text = private_text(&requests).to_lowercase();
        assert!(!saved(&text), "not saved: {text}");
        assert!(
            [
                "recurr",
                "already",
                "sent",
                "complet",
                "disabl",
                "enabl",
                "claim",
                "uncertain",
                "deliver",
                "run",
                "message",
                "one-off",
                "busy",
                "inspect"
            ]
            .iter()
            .any(|word| text.contains(word)),
            "actionable {id} refusal: {text}"
        );
    }
}
