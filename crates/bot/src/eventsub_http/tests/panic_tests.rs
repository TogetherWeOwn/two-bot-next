#![cfg(test)]

use super::*;

#[tokio::test]
async fn delivery_child_panic_is_reported_by_receiver_shutdown() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    struct PanickingLedger;
    impl LiveLedger for PanickingLedger {
        fn claim<'a>(&'a self, _claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome> {
            Box::pin(async { panic!("synthetic ledger panic") })
        }

        fn mark_started<'a>(
            &'a self,
            _dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async { Ok(false) })
        }

        fn mark_delivered<'a>(
            &'a self,
            _dedup_key: &'a str,
            _claim_token: &'a str,
            _message_id: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async { Ok(false) })
        }

        fn release_started<'a>(
            &'a self,
            _dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async { Ok(false) })
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = Arc::new(ReceiverState::with_delivery(
        &config(),
        Some(DeliveryDeps {
            ledger: Arc::new(PanickingLedger),
            sender: FakeSender::scripted(vec![]),
            config: LiveDeliveryConfig {
                broadcasters: vec!["123456".to_owned()],
                invite_url: "https://discord.gg/example".to_owned(),
                channel_id: "777".to_owned(),
            },
        }),
    ));
    let response = router(Arc::clone(&state))
        .oneshot(post_notification_request("synthetic-delivery-panic"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(BoundEventsubReceiver { listener, state }.serve(receiver));
    shutdown.send_replace(true);
    let error = tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .expect("receiver shutdown must join the panicked child")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "EventSub delivery task failed");
}
