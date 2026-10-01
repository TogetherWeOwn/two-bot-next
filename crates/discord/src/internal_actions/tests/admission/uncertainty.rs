use super::*;
use two_bot_core::backup::guild_config_api::GuildConfigDiscordApi;

async fn assert_unknown_fences_restart(db: &TestDatabase, mock: &MockDiscord) {
    let occupied: bool = sqlx::query_scalar("SELECT in_flight FROM public.discord_send_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(
        occupied,
        "complete exchange is not proof of a certain mutation"
    );
    let second = db.independent_pool().await.unwrap();
    let gate = Arc::new(PgSendAdmission::new(second.clone(), &token()).unwrap());
    let restarted =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    // A new sticky nonce/claim and a fresh transport cannot authorize replay.
    assert!(matches!(
        restarted
            .post_message(CHANNEL, "replacement", Some(2))
            .await,
        Err(DiscordError::Unavailable(_))
    ));
    let api_base = format!("{}/api/v10", mock.origin);
    let mut backup = GuildConfigDiscordApi::with_admission(
        Some(&api_base),
        None,
        token(),
        "fixture-app".to_owned(),
        "fixture-guild".to_owned(),
        gate.clone(),
    )
    .unwrap();
    assert!(backup
        .write("POST", "/guilds/111/roles", json!({"name": "fixture"}))
        .await
        .is_err());
    let announced = announcement_executor(mock, gate);
    assert_eq!(
        run_once(&announced, &announcement("independent intent")).await,
        ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked)
    );
    assert_eq!(
        mock.count(),
        1,
        "no wire attempt before authorized reconciliation"
    );
    second.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_action_uncertain_status_and_invalid_receipts_keep_cross_pool_fence() {
    for (status, body) in [
        (500, r#"{"message":"uncertain"}"#),
        (408, r#"{"message":"uncertain"}"#),
        (409, r#"{"message":"uncertain"}"#),
        (425, r#"{"message":"uncertain"}"#),
        (302, r#"{"message":"uncertain"}"#),
        (202, r#"{"id":"123"}"#),
        (200, "not JSON"),
        (201, "{}"),
        (201, r#"{"id":"0"}"#),
        (201, r#"{"id":"not-an-id"}"#),
    ] {
        let db = database().await;
        let mock = MockDiscord::start(Reply::new(status, body)).await;
        {
            let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
            let action =
                ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate).unwrap();
            assert!(matches!(
                action
                    .post_message(CHANNEL, "first sticky replacement", Some(1))
                    .await,
                Err(DiscordError::Unavailable(_))
            ));
        }
        assert_unknown_fences_restart(&db, &mock).await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_backup_uncertain_status_and_invalid_receipts_keep_cross_transport_fence() {
    for (status, body) in [
        (500, r#"{"message":"uncertain"}"#),
        (408, r#"{"message":"uncertain"}"#),
        (409, r#"{"message":"uncertain"}"#),
        (425, r#"{"message":"uncertain"}"#),
        (302, r#"{"message":"uncertain"}"#),
        (202, r#"{"id":"123"}"#),
        (200, "not JSON"),
        (201, "{}"),
        (201, r#"{"id":"0"}"#),
    ] {
        let db = database().await;
        let mock = MockDiscord::start(Reply::new(status, body)).await;
        {
            let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
            let api_base = format!("{}/api/v10", mock.origin);
            let mut backup = GuildConfigDiscordApi::with_admission(
                Some(&api_base),
                None,
                token(),
                "fixture-app".to_owned(),
                "fixture-guild".to_owned(),
                gate,
            )
            .unwrap();
            assert!(backup
                .write("POST", "/guilds/111/roles", json!({"name": "fixture"}))
                .await
                .is_err());
            assert_eq!(backup.writes, 0);
        }
        assert_unknown_fences_restart(&db, &mock).await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_valid_mutation_receipts_release_for_other_transports() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::success()).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    assert_eq!(
        action.post_message(CHANNEL, "fixture", None).await.unwrap(),
        MESSAGE
    );
    let api_base = format!("{}/api/v10", mock.origin);
    let mut backup = GuildConfigDiscordApi::with_admission(
        Some(&api_base),
        None,
        token(),
        "fixture-app".to_owned(),
        "fixture-guild".to_owned(),
        gate.clone(),
    )
    .unwrap();
    assert_eq!(
        backup
            .write("POST", "/guilds/111/roles", json!({"name": "fixture"}))
            .await
            .unwrap()
            .unwrap()["id"],
        MESSAGE
    );
    assert_eq!(backup.writes, 1);
    let announced = announcement_executor(&mock, gate);
    assert!(matches!(
        run_once(&announced, &announcement("fixture")).await,
        ExecutionOutcome::Posted(_)
    ));
    assert_eq!(mock.count(), 3);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_read_only_complete_5xx_does_not_hold_mutation_lane() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::new(500, "unavailable")).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), &token()).unwrap());
    let action =
        ActionExecutor::with_admission(token(), Some(mock.origin.clone()), gate.clone()).unwrap();
    for _ in 0..2 {
        assert_eq!(action.get_json_once("/users/@me").await.unwrap(), None);
    }
    let permit = gate
        .admit()
        .await
        .expect("read-only status has no uncertain mutation");
    permit.complete(None).await.unwrap();
    assert_eq!(mock.count(), 2);
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn admission_caller_authorization_cannot_bypass_another_tokens_held_lane() {
    let db = database().await;
    let mock = MockDiscord::start(Reply::success()).await;
    let token_a = token();
    let token_b = format!("{}-other", token_a);
    let gate_a = Arc::new(PgSendAdmission::new(db.pool().clone(), &token_a).unwrap());
    let gate_b = PgSendAdmission::new(db.pool().clone(), &token_b).unwrap();
    let held_b = gate_b.admit().await.unwrap();
    let transport = crate::executor::HyperTransport::with_admission(
        token_a.clone(),
        Some(mock.origin.clone()),
        gate_a.clone(),
    )
    .unwrap();
    let request =
        twilight_http::request::Request::builder(&twilight_http::routing::Route::CreateMessage {
            channel_id: CHANNEL.parse().unwrap(),
        })
        .headers(std::iter::once((
            hyper::header::AUTHORIZATION,
            hyper::header::HeaderValue::from_str(&format!("Bot {token_b}")).unwrap(),
        )))
        .body(br#"{"content":"fixture"}"#.to_vec())
        .build()
        .unwrap();
    assert_eq!(
        transport.send_request(&request).await.unwrap_err(),
        "caller-supplied authorization is forbidden"
    );
    assert_eq!(mock.count(), 0);
    // Rejection preceded admission as well as I/O; B remains occupied.
    gate_a.admit().await.unwrap().complete(None).await.unwrap();
    assert!(matches!(gate_b.admit().await, Err(AdmissionError::Blocked)));
    let action =
        ActionExecutor::with_admission(token_a.clone(), Some(mock.origin.clone()), gate_a).unwrap();
    action
        .post_message(CHANNEL, "bound token", None)
        .await
        .unwrap();
    assert_eq!(
        mock.requests.lock().unwrap()[0].authorization.as_deref(),
        Some(format!("Bot {token_a}").as_str())
    );
    assert_eq!(mock.count(), 1);
    drop(held_b); // no release on drop, even for an intentionally held fixture.
    db.close().await.unwrap();
}
