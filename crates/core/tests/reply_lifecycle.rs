//! Router response contract, with a paused clock and no external services.
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use two_bot_core::router::replies::{
    run_handler, InteractionReply, ReplyError, ReplyOperation, ReplyPolicy, ReplyTransport,
};

#[derive(Default)]
struct Transport {
    operations: Mutex<Vec<ReplyOperation>>,
    fail: bool,
}

impl Transport {
    fn operations(&self) -> Vec<ReplyOperation> {
        self.operations.lock().unwrap().clone()
    }
}

impl ReplyTransport for Transport {
    type Error = std::io::Error;

    async fn execute(&self, operation: ReplyOperation) -> Result<(), Self::Error> {
        self.operations.lock().unwrap().push(operation);
        if self.fail {
            Err(std::io::Error::other("transport unavailable"))
        } else {
            Ok(())
        }
    }
}

fn reply(content: &str, ephemeral: bool) -> InteractionReply {
    InteractionReply::new(content, ephemeral)
}

#[tokio::test(start_paused = true)]
async fn slow_handler_defers_at_default_budget_then_edits_without_cancelling() {
    for ephemeral in [true, false] {
        let transport = Transport::default();
        let started = tokio::time::Instant::now();
        run_handler(&transport, ReplyPolicy::default(), ephemeral, |_| async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            assert_eq!(
                transport.operations(),
                [ReplyOperation::Defer { ephemeral }]
            );
            assert_eq!(started.elapsed(), Duration::from_secs(4));
            Ok::<_, &str>(reply("completed", ephemeral))
        })
        .await
        .unwrap();
        assert_eq!(
            transport.operations(),
            [
                ReplyOperation::Defer { ephemeral },
                ReplyOperation::EditOriginal {
                    content: "completed".into()
                },
            ]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn fast_handler_replies_once_without_defer() {
    let transport = Transport::default();
    run_handler(&transport, ReplyPolicy::default(), true, |_| async {
        Ok::<_, &str>(reply("fast", true))
    })
    .await
    .unwrap();
    assert_eq!(
        transport.operations(),
        [ReplyOperation::Respond(reply("fast", true))]
    );
}

#[tokio::test(start_paused = true)]
async fn custom_budget_is_respected() {
    let transport = Transport::default();
    let budget = Duration::from_millis(250);
    let start = tokio::time::Instant::now();
    run_handler(
        &transport,
        ReplyPolicy::new(budget).unwrap(),
        true,
        |_| async {
            tokio::time::sleep(budget + Duration::from_millis(1)).await;
            assert_eq!(
                transport.operations(),
                [ReplyOperation::Defer { ephemeral: true }]
            );
            Ok::<_, &str>(reply("done", true))
        },
    )
    .await
    .unwrap();
    assert_eq!(start.elapsed(), budget + Duration::from_millis(1));
    assert!(ReplyPolicy::new(Duration::from_secs(3)).is_err());
}

#[tokio::test(start_paused = true)]
async fn handler_ack_and_timer_never_double_ack() {
    for early_reply in [false, true] {
        let transport = Transport::default();
        run_handler(
            &transport,
            ReplyPolicy::default(),
            true,
            |session| async move {
                if early_reply {
                    session.respond(reply("working", true)).await.unwrap();
                } else {
                    session.defer(true).await.unwrap();
                }
                tokio::time::sleep(Duration::from_secs(4)).await;
                session.defer(true).await.unwrap();
                Ok::<_, &str>(reply("done", true))
            },
        )
        .await
        .unwrap();
        let ops = transport.operations();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[1],
            ReplyOperation::EditOriginal {
                content: "done".into()
            }
        );
    }
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);
impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn error_content(ops: &[ReplyOperation]) -> &str {
    match ops.last().unwrap() {
        ReplyOperation::Respond(reply) | ReplyOperation::Followup(reply) => {
            assert!(reply.ephemeral);
            &reply.content
        }
        ReplyOperation::EditOriginal { content } => content,
        op => panic!("expected error reply, got {op:?}"),
    }
}

fn reference(content: &str) -> &str {
    let id = content
        .strip_prefix("Something went wrong (ref ")
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    assert_eq!(id.len(), 8);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    id
}

#[tokio::test(start_paused = true)]
async fn failure_is_private_redacted_and_logged_with_the_same_reference() {
    let transport = Transport::default();
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    run_handler(&transport, ReplyPolicy::default(), false, |_| async {
        Err::<InteractionReply, _>("private database detail")
    })
    .await
    .unwrap();
    let ops = transport.operations();
    assert_eq!(ops.len(), 1);
    let content = error_content(&ops);
    assert!(!content.contains("database"));
    let logs = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains(reference(content)),
        "same reference in logs: {logs}"
    );
    assert!(logs.contains("private database detail"));
}

#[tokio::test(start_paused = true)]
async fn deferred_failure_never_edits_internal_errors_into_a_public_message() {
    for ephemeral in [true, false] {
        let transport = Transport::default();
        run_handler(&transport, ReplyPolicy::default(), ephemeral, |_| async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            Err::<InteractionReply, _>("database detail")
        })
        .await
        .unwrap();
        let ops = transport.operations();
        assert_eq!(ops[0], ReplyOperation::Defer { ephemeral });
        if ephemeral {
            assert_eq!(ops.len(), 2);
            assert!(matches!(ops[1], ReplyOperation::EditOriginal { .. }));
        } else {
            assert_eq!(ops.len(), 3);
            assert_eq!(ops[1], ReplyOperation::DeleteOriginal);
            assert!(matches!(ops[2], ReplyOperation::Followup(_)));
        }
        reference(error_content(&ops));
    }
}

#[tokio::test(start_paused = true)]
async fn panics_in_future_creation_and_polling_are_isolated_and_redacted() {
    let transport = Transport::default();
    run_handler(
        &transport,
        ReplyPolicy::default(),
        false,
        |_| -> std::future::Ready<Result<InteractionReply, &str>> { panic!("construction detail") },
    )
    .await
    .unwrap();
    run_handler(&transport, ReplyPolicy::default(), false, |_| async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        panic!("poll detail");
        #[allow(unreachable_code)]
        Ok::<_, &str>(reply("unreachable", false))
    })
    .await
    .unwrap();
    // A subsequent interaction still runs on the same runtime.
    run_handler(&transport, ReplyPolicy::default(), false, |_| async {
        Ok::<_, &str>(reply("healthy", false))
    })
    .await
    .unwrap();
    let ops = transport.operations();
    assert!(matches!(&ops[0], ReplyOperation::Respond(r) if r.ephemeral));
    assert!(!error_content(&ops[..1]).contains("construction"));
    reference(error_content(&ops[..1]));
    assert_eq!(ops[1], ReplyOperation::Defer { ephemeral: false });
    assert_eq!(ops[2], ReplyOperation::DeleteOriginal);
    reference(error_content(&ops[..4]));
    assert_eq!(ops[4], ReplyOperation::Respond(reply("healthy", false)));
}

#[tokio::test(start_paused = true)]
async fn failure_after_an_early_reply_is_an_ephemeral_followup() {
    let transport = Transport::default();
    run_handler(
        &transport,
        ReplyPolicy::default(),
        false,
        |session| async move {
            session
                .respond(reply("already delivered", false))
                .await
                .unwrap();
            Err::<InteractionReply, _>("detail")
        },
    )
    .await
    .unwrap();
    let ops = transport.operations();
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[1], ReplyOperation::Followup(_)));
    reference(error_content(&ops));
}

#[tokio::test(start_paused = true)]
async fn callback_failure_propagates_and_is_not_retried() {
    let transport = Transport {
        fail: true,
        ..Default::default()
    };
    let result = run_handler(&transport, ReplyPolicy::default(), true, |_| async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        Ok::<_, &str>(reply("done", true))
    })
    .await;
    assert!(matches!(result, Err(ReplyError::Transport(_))));
    assert_eq!(transport.operations().len(), 1);
    let transport = Transport {
        fail: true,
        ..Default::default()
    };
    let result = run_handler(
        &transport,
        ReplyPolicy::default(),
        true,
        |session| async move {
            session.defer(true).await?;
            Ok::<_, ReplyError<std::io::Error>>(reply("done", true))
        },
    )
    .await;
    assert!(matches!(result, Err(ReplyError::DeliveryUncertain)));
    assert_eq!(transport.operations().len(), 1);
}

#[test]
fn text_is_capped_without_splitting_unicode() {
    let r = reply(&"🦀".repeat(2001), true);
    assert_eq!(r.content.chars().count(), 2000);
}
