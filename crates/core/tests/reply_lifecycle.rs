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
    sent_at: Mutex<Vec<tokio::time::Instant>>,
    delay: Duration,
    fail: bool,
    fail_edit: bool,
    original_deleted: Mutex<bool>,
}

impl Transport {
    fn operations(&self) -> Vec<ReplyOperation> {
        self.operations.lock().unwrap().clone()
    }
}

impl ReplyTransport for Transport {
    type Error = std::io::Error;

    async fn execute(&self, operation: ReplyOperation) -> Result<Option<u64>, Self::Error> {
        let creates_followup = matches!(operation, ReplyOperation::Followup(_));
        self.operations.lock().unwrap().push(operation.clone());
        match operation {
            ReplyOperation::DeleteOriginal => *self.original_deleted.lock().unwrap() = true,
            ReplyOperation::EditOriginal { .. } => {
                if *self.original_deleted.lock().unwrap() {
                    return Err(std::io::Error::other("original was deleted"));
                }
                if self.fail_edit {
                    return Err(std::io::Error::other("progress edit rejected"));
                }
            }
            _ => {}
        }
        self.sent_at
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail {
            Err(std::io::Error::other("transport unavailable"))
        } else {
            Ok(creates_followup.then_some(42))
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
            assert_eq!(
                transport.sent_at.lock().unwrap()[0] - started,
                Duration::from_secs(2)
            );
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
async fn deadline_during_handler_ack_keeps_polling_without_deadlock_or_double_ack() {
    for early_reply in [true, false] {
        let transport = Transport {
            delay: Duration::from_secs(3),
            ..Default::default()
        };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
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
                    Ok::<_, &str>(reply("completed", true))
                },
            ),
        )
        .await
        .unwrap();
        result.unwrap();
        let ops = transport.operations();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[1],
            ReplyOperation::EditOriginal {
                content: "completed".into()
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn completion_during_auto_defer_waits_for_the_ack_before_editing() {
    let transport = Transport {
        delay: Duration::from_secs(3),
        ..Default::default()
    };
    let start = tokio::time::Instant::now();
    run_handler(&transport, ReplyPolicy::default(), true, |_| async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        Ok::<_, &str>(reply("done", true))
    })
    .await
    .unwrap();
    assert_eq!(
        *transport.sent_at.lock().unwrap(),
        [
            start + Duration::from_secs(2),
            start + Duration::from_secs(5)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn private_completion_after_a_public_defer_uses_a_private_followup() {
    let transport = Transport::default();
    run_handler(&transport, ReplyPolicy::default(), false, |_| async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        Ok::<_, &str>(reply("private", true))
    })
    .await
    .unwrap();
    assert_eq!(
        transport.operations(),
        [
            ReplyOperation::Defer { ephemeral: false },
            ReplyOperation::DeleteOriginal,
            ReplyOperation::Followup(reply("private", true))
        ]
    );
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

// Use one subscriber for this test binary: thread-local subscribers can race
// global callsite interest updates from concurrently executing handler tests.
// Only capture/clear is serialized; other tests may also append fixture logs.
static LOG_CAPTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);
impl Log {
    fn capture() -> Self {
        static LOG: std::sync::OnceLock<Log> = std::sync::OnceLock::new();
        let log = LOG
            .get_or_init(|| {
                let log = Log::default();
                let writer = log.clone();
                tracing::subscriber::set_global_default(
                    tracing_subscriber::fmt()
                        .without_time()
                        .with_ansi(false)
                        .with_writer(move || writer.clone())
                        .finish(),
                )
                .unwrap();
                log
            })
            .clone();
        log.0.lock().unwrap().clear();
        log
    }
}
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
    let _capture = LOG_CAPTURE.lock().await;
    let transport = Transport::default();
    let log = Log::capture();
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

#[tokio::test(start_paused = true)]
async fn private_progress_after_public_defer_keeps_a_live_completion_target() {
    let transport = Transport::default();
    run_handler(
        &transport,
        ReplyPolicy::default(),
        false,
        |session| async move {
            session.defer(false).await?;
            session.respond(reply("private progress", true)).await?;
            session
                .respond(reply("more private progress", true))
                .await?;
            Ok::<_, ReplyError<std::io::Error>>(reply("final result", true))
        },
    )
    .await
    .expect("completion must not edit the deleted original");
    assert_eq!(
        transport.operations(),
        [
            ReplyOperation::Defer { ephemeral: false },
            ReplyOperation::DeleteOriginal,
            ReplyOperation::Followup(reply("private progress", true)),
            ReplyOperation::EditFollowup {
                message_id: 42,
                content: "more private progress".into()
            },
            ReplyOperation::EditFollowup {
                message_id: 42,
                content: "final result".into()
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn failed_progress_edit_preserves_ack_for_a_private_error_followup() {
    let transport = Transport {
        fail_edit: true,
        ..Default::default()
    };
    run_handler(
        &transport,
        ReplyPolicy::default(),
        false,
        |session| async move {
            session.respond(reply("working", false)).await?;
            session.respond(reply("more progress", false)).await?;
            Ok::<_, ReplyError<std::io::Error>>(reply("final result", false))
        },
    )
    .await
    .expect("a failed edit must not erase a known ACK");
    let ops = transport.operations();
    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[2], ReplyOperation::Followup(_)));
    reference(error_content(&ops));
}

#[tokio::test(start_paused = true)]
async fn completed_handler_failure_is_logged_even_when_inflight_defer_fails() {
    let _capture = LOG_CAPTURE.lock().await;
    for panic in [false, true] {
        let transport = Transport {
            fail: true,
            delay: Duration::from_secs(3),
            ..Default::default()
        };
        let log = Log::capture();
        let result = run_handler(&transport, ReplyPolicy::default(), false, |_| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert_eq!(transport.operations().len(), 1, "ACK is in flight");
            if panic {
                panic!("completed handler panic sentinel");
            }
            Err::<InteractionReply, _>("completed handler error sentinel")
        })
        .await;
        assert!(matches!(result, Err(ReplyError::Transport(_))));
        assert_eq!(
            transport.operations().len(),
            1,
            "never retry an uncertain ACK"
        );
        let logs = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("reference="),
            "correlation log missing: {logs}"
        );
        assert!(
            logs.contains(if panic {
                "completed handler panic sentinel"
            } else {
                "completed handler error sentinel"
            }),
            "completed failure lost: {logs}"
        );
    }
}

#[test]
fn text_is_capped_without_splitting_unicode() {
    let r = reply(&"🦀".repeat(2001), true);
    assert_eq!(r.content.chars().count(), 2000);
}
