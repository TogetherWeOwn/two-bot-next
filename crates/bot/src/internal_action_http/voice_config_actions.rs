//! Signed, guild-scoped voice configuration reads and durable saves.
//! The web caller owns dashboard-session and live Manage Server checks, exactly
//! as for settings actions. No actor or guild in a body grants authority here:
//! HMAC/caller/capability checks already ran, and the guild is server-selected.

use super::*;
use two_bot_core::settings::contains_json_nul;
use two_bot_core::voice_config::{
    decode_configuration, validate_configuration_values, VoiceConfiguration,
};
use two_bot_core::voice_logging::{parse_detail_level, LoggingSettings};
use two_bot_cutover::voice_rooms::PgRoomStore;

struct VoiceWrite {
    config: VoiceConfiguration,
    actor: String,
    expected_version: Option<i64>,
    logging: Option<LoggingSettings>,
}

impl VoiceWrite {
    fn parse(body: &Map<String, Value>, guild_id: &str) -> Result<Self, ActionError> {
        if body.keys().any(|key| {
            !matches!(
                key.as_str(),
                "action" | "config" | "updated_by" | "expected_version" | "logging_settings"
            )
        }) {
            return Err(malformed());
        }
        let actor = require_snowflake(body, "updated_by")?.to_owned();
        let value = body.get("config").ok_or_else(malformed)?;
        if contains_json_nul(value) {
            return Err(malformed());
        }
        let bytes = serde_json::to_vec(value).map_err(|_| malformed())?;
        let config = decode_configuration(&bytes).map_err(|_| malformed())?;
        validate_configuration_values(&config, guild_id).map_err(|_| malformed())?;
        let expected_version = body
            .get("expected_version")
            .map(|value| value.as_i64().ok_or_else(malformed))
            .transpose()?;
        let logging = body
            .get("logging_settings")
            .map(parse_logging)
            .transpose()?;
        Ok(Self {
            config,
            actor,
            expected_version,
            logging,
        })
    }
}

fn parse_logging(value: &Value) -> Result<LoggingSettings, ActionError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Document {
        level: String,
        channel_id: Option<String>,
        mention_role_id: Option<String>,
    }
    let document: Document = serde_json::from_value(value.clone()).map_err(|_| malformed())?;
    let id = |value: String| {
        if !two_bot_core::internal_actions::is_snowflake(&value) {
            return Err(malformed());
        }
        value.parse::<u64>().map_err(|_| malformed())
    };
    Ok(LoggingSettings {
        level: parse_detail_level(&document.level).map_err(|_| malformed())?,
        channel_id: document.channel_id.map(id).transpose()?,
        mention_role_id: document.mention_role_id.map(id).transpose()?,
    })
}

fn malformed() -> ActionError {
    ActionError::new(
        ErrorCode::Malformed,
        "Invalid voice configuration",
        "voice_config_malformed",
    )
}

fn store_failure(error: sqlx::Error) -> Failure {
    match error {
        sqlx::Error::RowNotFound => Failure::code(ErrorCode::VersionConflict),
        sqlx::Error::InvalidArgument(_) => Failure::code(ErrorCode::Malformed),
        // Never surface Postgres detail: it may contain submitted field values.
        _ => Failure::code(ErrorCode::Internal),
    }
}

pub(super) async fn handle(
    state: &ReceiverState,
    decision: &AuthDecision,
    idempotency_header: Option<&str>,
    raw: &[u8],
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let guild = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let Ok(guild_id) = guild.parse::<u64>() else {
        return reject(Failure::code(ErrorCode::Internal));
    };
    let store = PgRoomStore::new(state.store.pool().clone()).voice_configs();
    if decision.action == "voice.config.get" {
        if decision.body.keys().any(|field| field != "action") {
            return reject(Failure::from_action(malformed()));
        }
        return match store.snapshot_versioned(guild_id).await {
            Ok(snapshot) => {
                let logging = snapshot.logging_settings;
                metrics::global().internal_action(internal_family(action), "executed");
                settings_read_response(
                    json!({
                        "config": snapshot.config,
                        "logging_settings": {
                            "level": logging.level.as_str(),
                            "channel_id": logging.channel_id.map(|id| id.to_string()),
                            "mention_role_id": logging.mention_role_id.map(|id| id.to_string()),
                        },
                        "updated_by": snapshot.updated_by,
                        "updated_at": snapshot.updated_at,
                    }),
                    snapshot.version,
                    id,
                )
            }
            Err(error) => reject(store_failure(error)),
        };
    }
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let command = match VoiceWrite::parse(&decision.body, guild) {
        Ok(command) => command,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let (Ok(guild_id_subject), Ok(actor_id)) =
        (DiscordId::new(guild), DiscordId::new(&command.actor))
    else {
        return reject(Failure::from_action(malformed()));
    };
    let subject = AuditSubject {
        guild_id: Some(guild_id_subject),
        actor_id: Some(actor_id),
        ..AuditSubject::default()
    };
    let Some(caller) = state.config.caller_for(&decision.key_id) else {
        return reject(Failure::code(ErrorCode::Internal));
    };
    let identity = match RequestIdentity::new(caller, idempotency, &decision.action, raw) {
        Ok(identity) => identity,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let claim = match state.store.claim(&identity, &subject).await {
        Ok(InternalClaim::Claimed(claim)) => claim,
        Ok(InternalClaim::Replay(response)) => {
            return match response {
                TerminalResponse::Success { .. } => {
                    match state.store.voice_config_version(&identity).await {
                        Ok(version) => {
                            settings_write_response(json!({"outcome": "saved"}), version, id, true)
                        }
                        Err(_) => reject(Failure::reconciliation()),
                    }
                }
                TerminalResponse::Failure(_) => state.terminal(response, true, id, key, action),
            };
        }
        Ok(InternalClaim::Mismatch) => return reject(Failure::code(ErrorCode::VersionConflict)),
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress)),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation()),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    match store
        .apply_versioned(
            guild_id,
            &command.config,
            &command.actor,
            command.expected_version,
            command.logging.as_ref(),
        )
        .await
    {
        Ok(version) => {
            let terminal = TerminalResponse::Success {
                resource_id: None,
                affected: 1,
                outcome: None,
                stop_reason: None,
            };
            if let Err(error) = state
                .store
                .finish_voice_config(&claim, &terminal, version)
                .await
            {
                log_terminal_persist_failed(action, "finish", error);
                mark_unknown_best_effort(&state.store, &claim, action).await;
                return reject(Failure::reconciliation());
            }
            metrics::global().internal_action(internal_family(action), "executed");
            settings_write_response(json!({"outcome": "saved"}), version, id, false)
        }
        Err(error) => {
            let failure = store_failure(error);
            let terminal = match failure.class_code {
                ErrorCode::VersionConflict => Some(TerminalFailure::VersionConflict),
                ErrorCode::Malformed => Some(TerminalFailure::Malformed),
                _ => None,
            };
            if let Some(terminal) = terminal {
                if let Err(error) = state
                    .store
                    .finish(&claim, &TerminalResponse::Failure(terminal))
                    .await
                {
                    log_terminal_persist_failed(action, "finish", error);
                    mark_unknown_best_effort(&state.store, &claim, action).await;
                    return reject(Failure::reconciliation());
                }
                reject(failure)
            } else {
                mark_unknown_best_effort(&state.store, &claim, action).await;
                reject(Failure::reconciliation())
            }
        }
    }
}
