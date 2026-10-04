use std::{
    future::pending,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

use serde_json::Value;
use two_bot_core::router::replies::ReplyOperation;

use super::*;

const FIXTURES: &str = include_str!("../../tests/fixtures/staging_slash_smoke.json");

fn config() -> SmokeConfig<'static> {
    SmokeConfig {
        application_id: Some(STAGING_BOT_APPLICATION_ID),
        guild_id: Some(TWO_STAGING_GUILD_ID),
        ..Default::default()
    }
}

#[derive(Default)]
struct RecordingTransport {
    operations: Mutex<Vec<ReplyOperation>>,
    fail: bool,
    hang: bool,
}

impl ReplyTransport for RecordingTransport {
    type Error = std::io::Error;

    async fn execute(&self, operation: ReplyOperation) -> Result<Option<u64>, Self::Error> {
        self.operations.lock().unwrap().push(operation);
        if self.hang {
            pending::<()>().await;
        }
        if self.fail {
            return Err(std::io::Error::other("fixture-remote-error-sensitive"));
        }
        Ok(None)
    }
}

struct RecordingFixtures {
    data: Value,
    calls: Mutex<Vec<SmokeStep>>,
    dropped: AtomicUsize,
    down: bool,
    hang: bool,
    wrong_shape: bool,
    wrong_guild: bool,
}

impl RecordingFixtures {
    fn new(case: &str) -> Self {
        let all: Value = serde_json::from_str(FIXTURES).unwrap();
        Self {
            data: all[case].clone(),
            calls: Mutex::new(Vec::new()),
            dropped: AtomicUsize::new(0),
            down: false,
            hang: false,
            wrong_shape: false,
            wrong_guild: false,
        }
    }

    fn profile(&self) -> LevelProfile {
        LevelProfile {
            guild_id: if self.wrong_guild {
                LIVE_GUILD_ID
            } else {
                TWO_STAGING_GUILD_ID
            }
            .into(),
            member_id: "7".into(),
            xp: self.data["xp"].as_u64().unwrap(),
            level: self.data["level"].as_u64().unwrap(),
            message_xp: 0,
            voice_xp: 0,
            imported_xp: 0,
            rank: self.data["rank"].as_u64(),
            member_count: self.data["member_count"].as_u64().unwrap(),
            next_level_xp: 0,
        }
    }

    fn entries(&self) -> Vec<LeaderboardEntry> {
        self.data["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| LeaderboardEntry {
                member_id: row["member_id"].as_str().unwrap().into(),
                xp: row["xp"].as_u64().unwrap(),
                level: row["level"].as_u64().unwrap(),
                rank: row["rank"].as_u64().unwrap(),
            })
            .collect()
    }
}

struct DropProbe<'a>(&'a AtomicUsize);
impl Drop for DropProbe<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl SmokeFixtures for RecordingFixtures {
    async fn load(&self, step: SmokeStep) -> Result<FixtureObservation, FixtureDown> {
        self.calls.lock().unwrap().push(step);
        let _probe = DropProbe(&self.dropped);
        if self.hang {
            pending::<()>().await;
        }
        if self.down {
            return Err(FixtureDown);
        }
        if self.wrong_shape {
            return Ok(FixtureObservation::Health {
                healthy: true,
                ready: true,
            });
        }
        Ok(match step {
            SmokeStep::Rank => FixtureObservation::Rank {
                profile: self.profile(),
                display_name: self.data["name"].as_str().unwrap().into(),
            },
            SmokeStep::Leaderboard => FixtureObservation::Leaderboard(self.entries()),
            SmokeStep::Health => FixtureObservation::Health {
                healthy: self.data["healthy"].as_bool().unwrap(),
                ready: self.data["ready"].as_bool().unwrap(),
            },
            _ => panic!("unsupported command must not load a fixture"),
        })
    }
}

#[tokio::test]
async fn live_guild_and_bad_configuration_refuse_before_all_calls() {
    let mut cases = vec![
        (SmokeConfig::default(), Refusal::MissingApplication),
        (
            SmokeConfig {
                application_id: Some(STAGING_BOT_APPLICATION_ID),
                ..Default::default()
            },
            Refusal::MissingGuild,
        ),
        (
            SmokeConfig {
                guild_id: Some(LIVE_GUILD_ID),
                ..config()
            },
            Refusal::LiveGuild,
        ),
        (
            SmokeConfig {
                application_id: Some("0"),
                ..config()
            },
            Refusal::WrongApplication,
        ),
        (
            SmokeConfig {
                application_id: Some("fixture-config-sensitive"),
                ..config()
            },
            Refusal::WrongApplication,
        ),
        (
            SmokeConfig {
                live_execution: true,
                ..config()
            },
            Refusal::LiveExecutionDisabled,
        ),
        (
            SmokeConfig {
                step_timeout: Duration::ZERO,
                ..config()
            },
            Refusal::InvalidTimeout,
        ),
        (
            SmokeConfig {
                step_timeout: Duration::from_secs(6),
                ..config()
            },
            Refusal::InvalidTimeout,
        ),
    ];
    for guild in [
        "",
        "0",
        "1545644954272137297 ",
        "01545644954272137297",
        "999",
        "fixture-config-sensitive",
    ] {
        cases.push((
            SmokeConfig {
                guild_id: Some(guild),
                ..config()
            },
            Refusal::WrongGuild,
        ));
    }
    for (config, expected) in cases {
        let source = RecordingFixtures::new("populated");
        let transport = RecordingTransport::default();
        let report = run_offline(&config, &source, &transport).await;
        assert_eq!(report.refusal, Some(expected));
        assert_eq!(report.verdict, OfflineVerdict::Refused);
        assert!(report.commands.is_empty());
        assert!(source.calls.lock().unwrap().is_empty());
        assert!(transport.operations.lock().unwrap().is_empty());
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("fixture-config-sensitive"));
    }
}

#[tokio::test]
async fn live_guild_fence_precedes_missing_application_and_live_opt_in() {
    let source = RecordingFixtures::new("populated");
    let transport = RecordingTransport::default();
    let config = SmokeConfig {
        guild_id: Some(LIVE_GUILD_ID),
        live_execution: true,
        ..Default::default()
    };
    let report = run_offline(&config, &source, &transport).await;
    assert_eq!(report.refusal, Some(Refusal::LiveGuild));
    assert!(source.calls.lock().unwrap().is_empty());
    assert!(transport.operations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn populated_and_empty_fixtures_use_compiled_routes_and_real_reply_builders() {
    for case in ["populated", "empty"] {
        let source = RecordingFixtures::new(case);
        let transport = RecordingTransport::default();
        let report = run_offline(&config(), &source, &transport).await;
        assert!(report.mock);
        assert!(!report.live_execution);
        assert_eq!(report.transport, "local-fixtures");
        // Supported fixtures passed, but source-plan /help and /ping are not
        // core commands. This must never become a complete staging PASS.
        assert_eq!(report.verdict, OfflineVerdict::Incomplete);
        assert_eq!(
            report
                .commands
                .iter()
                .map(|s| (s.name, s.result, s.actual))
                .collect::<Vec<_>>(),
            vec![
                ("/rank", StepResult::Pass, Observation::ReplyValidated),
                (
                    "/leaderboard",
                    StepResult::Pass,
                    Observation::ReplyValidated
                ),
                (
                    "/help",
                    StepResult::Skipped,
                    Observation::UnsupportedCoreCommand
                ),
                (
                    "/ping",
                    StepResult::Skipped,
                    Observation::VoiceCommandOutOfScope
                ),
                (
                    "health/readiness fixture",
                    StepResult::Pass,
                    Observation::HealthReady
                ),
            ]
        );
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec![SmokeStep::Rank, SmokeStep::Leaderboard, SmokeStep::Health]
        );
        let operations = transport.operations.lock().unwrap();
        let rank = rank_reply(&source.profile(), source.data["name"].as_str().unwrap());
        assert_eq!(
            operations[0],
            ReplyOperation::Respond(InteractionReply::new(rank.content, true))
        );
        assert_eq!(
            operations[1],
            ReplyOperation::Respond(InteractionReply::new(
                leaderboard_reply(&source.entries()).content,
                false
            ))
        );
        for operation in operations.iter() {
            let ReplyOperation::Respond(reply) = operation else {
                panic!("immediate fixture reply")
            };
            let wire = crate::text_response(reply.clone());
            let mentions = wire.data.unwrap().allowed_mentions.unwrap();
            assert!(mentions.parse.is_empty());
            assert!(mentions.users.is_empty());
            assert!(mentions.roles.is_empty());
        }
        let json = serde_json::to_string(&report).unwrap();
        let debug = format!("{report:?}");
        for sentinel in [
            "fixture-display-sensitive",
            "fixture-empty-member",
            STAGING_BOT_APPLICATION_ID,
            TWO_STAGING_GUILD_ID,
        ] {
            assert!(!json.contains(sentinel));
            assert!(!debug.contains(sentinel));
        }
        assert!(!json.contains("deployment"));
        assert!(report
            .commands
            .iter()
            .all(|s| s.failure_signature.is_none()));
    }
}

#[tokio::test]
async fn healthy_but_unready_and_down_health_are_failures() {
    for case in ["unready", "down"] {
        let source = RecordingFixtures::new(case);
        let transport = RecordingTransport::default();
        let report = run_offline(&config(), &source, &transport).await;
        assert_eq!(report.verdict, OfflineVerdict::Fail);
        assert_eq!(report.commands[4].actual, Observation::Down);
        assert_eq!(report.commands[4].result, StepResult::Fail);
        assert!(report.commands[4].failure_signature.is_some());
        assert_eq!(transport.operations.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn fixture_source_down_never_replies_or_masks_failure() {
    let mut source = RecordingFixtures::new("populated");
    source.down = true;
    let transport = RecordingTransport::default();
    let report = run_offline(&config(), &source, &transport).await;
    assert_eq!(report.verdict, OfflineVerdict::Fail);
    assert_eq!(
        report
            .commands
            .iter()
            .filter(|s| s.actual == Observation::Down)
            .count(),
        3
    );
    assert!(transport.operations.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn pending_fixture_times_out_is_dropped_and_remaining_steps_are_recorded() {
    let mut source = RecordingFixtures::new("populated");
    source.hang = true;
    let transport = RecordingTransport::default();
    let config = SmokeConfig {
        step_timeout: Duration::from_millis(50),
        ..config()
    };
    let report = run_offline(&config, &source, &transport).await;
    assert_eq!(report.verdict, OfflineVerdict::Fail);
    assert_eq!(report.commands.len(), 5);
    for index in [0, 1, 4] {
        assert_eq!(report.commands[index].actual, Observation::Timeout);
        assert_eq!(report.commands[index].duration_ms, 50);
    }
    assert_eq!(source.dropped.load(Ordering::SeqCst), 3);
    assert!(transport.operations.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn reply_timeout_is_bounded_without_callback_retry() {
    let source = RecordingFixtures::new("populated");
    let transport = RecordingTransport {
        hang: true,
        ..Default::default()
    };
    let config = SmokeConfig {
        step_timeout: Duration::from_millis(50),
        ..config()
    };
    let report = run_offline(&config, &source, &transport).await;
    assert_eq!(report.commands[0].actual, Observation::Timeout);
    assert_eq!(report.commands[1].actual, Observation::Timeout);
    assert_eq!(report.commands[4].actual, Observation::HealthReady);
    assert_eq!(report.verdict, OfflineVerdict::Fail);
    assert_eq!(transport.operations.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn reply_errors_are_redacted_and_not_retried() {
    let source = RecordingFixtures::new("populated");
    let transport = RecordingTransport {
        fail: true,
        ..Default::default()
    };
    let report = run_offline(&config(), &source, &transport).await;
    assert_eq!(report.commands[0].actual, Observation::ReplyTransportFailed);
    assert_eq!(report.commands[1].actual, Observation::ReplyTransportFailed);
    assert_eq!(report.verdict, OfflineVerdict::Fail);
    assert_eq!(transport.operations.lock().unwrap().len(), 2);
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("fixture-remote-error-sensitive"));
    assert!(!format!("{report:?}").contains("fixture-remote-error-sensitive"));
}

#[tokio::test]
async fn wrong_fixture_type_and_foreign_rank_model_fail_without_rank_reply() {
    for wrong_shape in [false, true] {
        let mut source = RecordingFixtures::new("populated");
        source.wrong_shape = wrong_shape;
        source.wrong_guild = !wrong_shape;
        let transport = RecordingTransport::default();
        let report = run_offline(&config(), &source, &transport).await;
        assert_eq!(report.commands[0].actual, Observation::FixtureMismatch);
        assert_eq!(report.verdict, OfflineVerdict::Fail);
        assert_eq!(
            transport.operations.lock().unwrap().len(),
            usize::from(!wrong_shape)
        );
    }
}

#[tokio::test]
async fn simultaneous_offline_runs_keep_independent_receipts() {
    let first = RecordingFixtures::new("populated");
    let second = RecordingFixtures::new("empty");
    let first_transport = RecordingTransport::default();
    let second_transport = RecordingTransport::default();
    let config = config();
    let (a, b) = tokio::join!(
        run_offline(&config, &first, &first_transport),
        run_offline(&config, &second, &second_transport),
    );
    assert_eq!(a.verdict, OfflineVerdict::Incomplete);
    assert_eq!(b.verdict, OfflineVerdict::Incomplete);
    assert_eq!(first_transport.operations.lock().unwrap().len(), 2);
    assert_eq!(second_transport.operations.lock().unwrap().len(), 2);
}
