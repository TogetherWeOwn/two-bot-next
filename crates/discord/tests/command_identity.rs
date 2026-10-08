use serde_json::json;
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::Interaction,
    },
    id::Id,
};
use two_bot_core::CommandDefinition;
use two_bot_discord::{command_identity::CommandIdentities, publish_commands};

fn registered(name: &str, id: Option<u64>) -> Command {
    let mut command =
        publish_commands(&[CommandDefinition::new(name, "Synthetic command.")]).remove(0);
    command.id = id.map(Id::new);
    command.application_id = Some(Id::new(1111));
    command.guild_id = Some(Id::new(2222));
    command
}

fn interaction(id: u64, name: &str, registration_guild: Option<u64>) -> Interaction {
    serde_json::from_value(json!({
        "id": "7000", "application_id": "1111", "type": 2, "token": "identity-fixture",
        "authorizing_integration_owners": {}, "entitlements": [], "guild_id": "2222",
        "data": {"id": id.to_string(), "name": name, "type": 1,
            "guild_id": registration_guild.map(|guild| guild.to_string())}
    }))
    .unwrap()
}

#[test]
fn registered_id_wins_and_unknown_ids_cannot_spoof_known_names() {
    let identities = CommandIdentities::default();
    identities
        .replace_guild(
            1111,
            2222,
            &[
                registered("ping", Some(9001)),
                registered("help", Some(9002)),
            ],
        )
        .unwrap();
    assert_eq!(
        identities
            .slash_name(&interaction(9001, "help", Some(2222)))
            .as_deref(),
        Some("ping")
    );
    assert_eq!(
        identities
            .slash_name(&interaction(9002, "ping", Some(2222)))
            .as_deref(),
        Some("help")
    );
    assert_eq!(
        identities.slash_name(&interaction(9999, "ping", Some(2222))),
        None
    );
    assert_eq!(
        identities.slash_name(&interaction(9999, "unpublished", Some(2222))),
        None
    );
}

#[test]
fn fallback_is_limited_to_unknown_registration_identity() {
    let identities = CommandIdentities::default();
    assert_eq!(
        identities
            .slash_name(&interaction(9001, "ping", Some(2222)))
            .as_deref(),
        Some("ping")
    );
    identities
        .replace_guild(
            1111,
            2222,
            &[registered("ping", None), registered("help", Some(9002))],
        )
        .unwrap();
    assert_eq!(
        identities
            .slash_name(&interaction(9001, "ping", Some(2222)))
            .as_deref(),
        Some("ping")
    );
    assert_eq!(
        identities
            .slash_name(&interaction(9002, "ping", Some(2222)))
            .as_deref(),
        Some("help")
    );
    // Global commands are not hydrated by a guild publication.
    assert_eq!(
        identities
            .slash_name(&interaction(9003, "ping", None))
            .as_deref(),
        Some("ping")
    );
    assert_eq!(
        identities.slash_name(&interaction(9002, "ping", None)),
        None
    );
}

#[test]
fn replacement_and_empty_snapshot_remove_old_ids_without_reopening_fallback() {
    let identities = CommandIdentities::default();
    let shared = identities.clone();
    identities
        .replace_guild(1111, 2222, &[registered("ping", Some(9001))])
        .unwrap();
    identities
        .replace_guild(1111, 2222, &[registered("ping", Some(9004))])
        .unwrap();
    assert_eq!(
        shared.slash_name(&interaction(9001, "ping", Some(2222))),
        None
    );
    assert_eq!(
        shared
            .slash_name(&interaction(9004, "old-name", Some(2222)))
            .as_deref(),
        Some("ping")
    );
    identities.replace_guild(1111, 2222, &[]).unwrap();
    assert_eq!(
        shared.slash_name(&interaction(9004, "ping", Some(2222))),
        None
    );
}

#[test]
fn command_type_application_and_registration_guild_are_fenced() {
    let identities = CommandIdentities::default();
    let mut context_menu = registered("ping", Some(9002));
    context_menu.kind = CommandType::User;
    identities
        .replace_guild(1111, 2222, &[registered("ping", Some(9001)), context_menu])
        .unwrap();
    assert_eq!(
        identities.slash_name(&interaction(9002, "ping", Some(2222))),
        None
    );
    for (key, value) in [("application_id", "3333"), ("guild_id", "4444")] {
        let mut payload = serde_json::to_value(interaction(9001, "ping", Some(2222))).unwrap();
        payload[key] = json!(value);
        assert_eq!(
            identities.slash_name(&serde_json::from_value(payload).unwrap()),
            None
        );
    }
    let mut payload = serde_json::to_value(interaction(9001, "ping", Some(2222))).unwrap();
    payload["data"]["type"] = json!(2);
    assert_eq!(
        identities.slash_name(&serde_json::from_value(payload).unwrap()),
        None
    );
    // A known command ID cannot acquire fallback in an unsynced guild.
    payload = serde_json::to_value(interaction(9001, "ping", Some(2222))).unwrap();
    payload["guild_id"] = json!("4444");
    payload["data"]["guild_id"] = json!("4444");
    assert_eq!(
        identities.slash_name(&serde_json::from_value(payload).unwrap()),
        None
    );
}

#[test]
fn invalid_receipts_preserve_the_last_successful_snapshot() {
    let identities = CommandIdentities::default();
    identities
        .replace_guild(1111, 2222, &[registered("ping", Some(9001))])
        .unwrap();
    for invalid in [
        vec![
            registered("ping", Some(9002)),
            registered("help", Some(9002)),
        ],
        vec![
            registered("ping", Some(9002)),
            registered("ping", Some(9003)),
        ],
    ] {
        assert!(identities.replace_guild(1111, 2222, &invalid).is_err());
        assert_eq!(
            identities
                .slash_name(&interaction(9001, "wrong-name", Some(2222)))
                .as_deref(),
            Some("ping")
        );
    }
    let mut wrong_guild = registered("ping", Some(9002));
    wrong_guild.guild_id = Some(Id::new(4444));
    assert!(identities
        .replace_guild(1111, 2222, &[wrong_guild])
        .is_err());
    assert_eq!(
        identities
            .slash_name(&interaction(9001, "wrong-name", Some(2222)))
            .as_deref(),
        Some("ping")
    );
}
