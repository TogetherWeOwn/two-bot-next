//! Offline staging smoke foundation. No HTTP client, token loader or live mode.
//!
//! Reuses the compiled core registry, dispatcher and leveling reply builders.
//! Fixture success is not deployed-health, gateway or database acceptance.

use std::{convert::Infallible, future::Future, time::Duration};

use serde::Serialize;
use tokio::time::{timeout, Instant};
use twilight_model::{
    application::{
        command::CommandType,
        interaction::{
            application_command::CommandData, Interaction, InteractionData, InteractionType,
        },
    },
    id::Id,
    oauth::ApplicationIntegrationMap,
};
use two_bot_core::{
    backup::guild_config::{LIVE_GUILD_ID, STAGING_BOT_APPLICATION_ID, TWO_STAGING_GUILD_ID},
    leveling::{
        leaderboard_reply, level_for_xp, rank_reply, total_xp_for_level, LeaderboardEntry,
        LevelProfile, MAX_STORED_XP,
    },
    router::replies::{InteractionReply, ReplyTransport},
    HandlerId, InteractionRouter, RouterGates, SlashOutcome,
};

use crate::{dispatch_interaction, route_interaction, DispatchOptions, RoutedInteraction};

/// Explicit identities only: no process-environment defaults or live override.
pub struct SmokeConfig<'a> {
    pub application_id: Option<&'a str>,
    pub guild_id: Option<&'a str>,
    pub live_execution: bool,
    pub step_timeout: Duration,
}

impl Default for SmokeConfig<'_> {
    fn default() -> Self {
        Self {
            application_id: None,
            guild_id: None,
            live_execution: false,
            step_timeout: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    LiveGuild,
    MissingApplication,
    MissingGuild,
    WrongApplication,
    WrongGuild,
    LiveExecutionDisabled,
    InvalidTimeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeStep {
    Rank,
    Leaderboard,
    Help,
    Ping,
    Health,
}

impl SmokeStep {
    fn name(self) -> &'static str {
        match self {
            Self::Rank => "/rank",
            Self::Leaderboard => "/leaderboard",
            Self::Help => "/help",
            Self::Ping => "/ping",
            Self::Health => "health/readiness fixture",
        }
    }
}

/// Local read models, never remote HTTP bodies. Health requires readiness too.
pub enum FixtureObservation {
    Rank {
        profile: LevelProfile,
        display_name: String,
    },
    Leaderboard(Vec<LeaderboardEntry>),
    Health {
        healthy: bool,
        ready: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct FixtureDown;

/// Implement with local fixtures only. There is deliberately no live adapter.
pub trait SmokeFixtures: Sync {
    fn load(
        &self,
        step: SmokeStep,
    ) -> impl Future<Output = Result<FixtureObservation, FixtureDown>> + Send;
}

/// Create a fresh local, interaction-scoped transport for each slash step.
/// Never return a shared single-interaction transport or connect a live adapter.
pub trait SmokeTransports: Sync {
    fn for_interaction(&self, interaction_id: u64) -> impl ReplyTransport + '_;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepResult {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Observation {
    ReplyValidated,
    HealthReady,
    Down,
    Timeout,
    FixtureMismatch,
    RouterMismatch,
    ReplyTransportFailed,
    CoveredByOfflineTests,
    VoiceCommandOutOfScope,
}

/// The command/result/duration/actual fields reuse the staging run-record
/// vocabulary. No invented UTC timestamp, deployment SHA or deploy run ID.
#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub name: &'static str,
    pub duration_ms: u64,
    pub result: StepResult,
    pub actual: Observation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_signature: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfflineVerdict {
    Incomplete,
    Fail,
    Refused,
}

/// A partial offline receipt, not a complete staging E2E run record. Unsupported
/// help/ping remain visible; no output can claim a live staging PASS.
#[derive(Debug, Clone, Serialize)]
pub struct SmokeReport {
    pub mock: bool,
    pub transport: &'static str,
    pub live_execution: bool,
    pub refusal: Option<Refusal>,
    pub commands: Vec<StepReport>,
    pub verdict: OfflineVerdict,
}

fn validate(config: &SmokeConfig<'_>) -> Result<(u64, u64), Refusal> {
    if config.guild_id == Some(LIVE_GUILD_ID) {
        return Err(Refusal::LiveGuild);
    }
    let application = config.application_id.ok_or(Refusal::MissingApplication)?;
    let guild = config.guild_id.ok_or(Refusal::MissingGuild)?;
    if application != STAGING_BOT_APPLICATION_ID {
        return Err(Refusal::WrongApplication);
    }
    if guild != TWO_STAGING_GUILD_ID {
        return Err(Refusal::WrongGuild);
    }
    if config.live_execution {
        return Err(Refusal::LiveExecutionDisabled);
    }
    if config.step_timeout.is_zero() || config.step_timeout > Duration::from_secs(5) {
        return Err(Refusal::InvalidTimeout);
    }
    Ok((
        application.parse().expect("canonical staging application"),
        guild.parse().expect("canonical staging guild"),
    ))
}

fn valid_rank_profile(profile: &LevelProfile) -> bool {
    if profile.guild_id != TWO_STAGING_GUILD_ID || profile.xp > MAX_STORED_XP {
        return false;
    }
    // Bound XP before curve evaluation, and use its canonical level rather than
    // evaluating unchecked arithmetic on the fixture's arbitrary level.
    let level = level_for_xp(profile.xp);
    profile.level == level && profile.next_level_xp == total_xp_for_level(level + 1)
}

#[allow(deprecated)]
fn synthetic_slash(name: &str, application: u64, guild: u64, interaction_id: u64) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(application),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: None,
            user: None,
        },
        channel: None,
        channel_id: None,
        context: None,
        data: Some(InteractionData::ApplicationCommand(Box::new(CommandData {
            guild_id: None,
            id: Id::new(1),
            name: name.to_owned(),
            kind: CommandType::ChatInput,
            options: Vec::new(),
            resolved: None,
            target_id: None,
        }))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: Some(Id::new(guild)),
        guild_locale: None,
        id: Id::new(interaction_id),
        kind: InteractionType::ApplicationCommand,
        locale: None,
        member: None,
        message: None,
        token: String::new(),
        user: None,
    }
}

async fn exercise<T: SmokeTransports, S: SmokeFixtures>(
    step: SmokeStep,
    router: &InteractionRouter,
    application: u64,
    guild: u64,
    fixtures: &S,
    transports: &T,
) -> Observation {
    // /help is a compiled core command now, but it needs no fixture: offline
    // router and renderer tests pin it, so this fixture-driven smoke records it
    // as skipped rather than inventing an interaction. /ping is still not a
    // core command.
    if step == SmokeStep::Help {
        return Observation::CoveredByOfflineTests;
    }
    if step == SmokeStep::Ping {
        return Observation::VoiceCommandOutOfScope;
    }
    let observation = match fixtures.load(step).await {
        Ok(observation) => observation,
        Err(FixtureDown) => return Observation::Down,
    };
    let (handler, reply, interaction_id) = match (step, observation) {
        (SmokeStep::Health, FixtureObservation::Health { healthy, ready }) => {
            return if healthy && ready {
                Observation::HealthReady
            } else {
                Observation::Down
            };
        }
        (
            SmokeStep::Rank,
            FixtureObservation::Rank {
                profile,
                display_name,
            },
        ) => {
            if !valid_rank_profile(&profile) {
                return Observation::FixtureMismatch;
            }
            let reply = rank_reply(&profile, &display_name);
            (
                HandlerId::Rank,
                InteractionReply::new(reply.content, reply.ephemeral),
                1,
            )
        }
        (SmokeStep::Leaderboard, FixtureObservation::Leaderboard(entries)) => (
            HandlerId::Leaderboard,
            InteractionReply::new(leaderboard_reply(&entries).content, false),
            2,
        ),
        _ => return Observation::FixtureMismatch,
    };
    let name = step.name().trim_start_matches('/');
    let interaction = synthetic_slash(name, application, guild, interaction_id);
    if !router
        .publish_set(&[])
        .is_ok_and(|definitions| definitions.iter().any(|definition| definition.name == name))
        || !matches!(route_interaction(router, &interaction, None),
            RoutedInteraction::Slash { outcome: SlashOutcome::Handled { handler: routed }, .. }
            if routed == handler)
    {
        return Observation::RouterMismatch;
    }
    let transport = transports.for_interaction(interaction.id.get());
    match dispatch_interaction(
        router,
        &interaction,
        &transport,
        DispatchOptions {
            ephemeral: reply.ephemeral,
            ..Default::default()
        },
        |_, _| async { Ok::<_, Infallible>(reply) },
    )
    .await
    {
        Ok(true) => Observation::ReplyValidated,
        Ok(false) => Observation::RouterMismatch,
        Err(_) => Observation::ReplyTransportFailed,
    }
}

/// Fence all configuration before even calling the fixture source. Execution
/// is sequential and bounded; a timed-out reply is never retried or detached.
pub async fn run_offline<T: SmokeTransports, S: SmokeFixtures>(
    config: &SmokeConfig<'_>,
    fixtures: &S,
    transports: &T,
) -> SmokeReport {
    let mut report = SmokeReport {
        mock: true,
        transport: "local-fixtures",
        live_execution: false,
        refusal: None,
        commands: Vec::new(),
        verdict: OfflineVerdict::Incomplete,
    };
    let (application, guild) = match validate(config) {
        Ok(ids) => ids,
        Err(refusal) => {
            report.refusal = Some(refusal);
            report.verdict = OfflineVerdict::Refused;
            return report;
        }
    };
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(guild),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    for step in [
        SmokeStep::Rank,
        SmokeStep::Leaderboard,
        SmokeStep::Help,
        SmokeStep::Ping,
        SmokeStep::Health,
    ] {
        let started = Instant::now();
        let actual = timeout(
            config.step_timeout,
            exercise(step, &router, application, guild, fixtures, transports),
        )
        .await
        .unwrap_or(Observation::Timeout);
        let result = match actual {
            Observation::ReplyValidated | Observation::HealthReady => StepResult::Pass,
            Observation::CoveredByOfflineTests | Observation::VoiceCommandOutOfScope => {
                StepResult::Skipped
            }
            _ => StepResult::Fail,
        };
        if result == StepResult::Fail {
            report.verdict = OfflineVerdict::Fail;
        }
        report.commands.push(StepReport {
            name: step.name(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            result,
            actual,
            failure_signature: (result == StepResult::Fail).then_some("offline-smoke-failure"),
        });
    }
    report
}

#[cfg(test)]
mod tests;
