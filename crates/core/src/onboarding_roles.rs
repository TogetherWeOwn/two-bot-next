//! Strict, bounded onboarding role configuration shared by writes and runtime reads.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admin_command_is_published_with_manage_server_and_bounded_inputs() {
        let commands = crate::commands::core_commands();
        let command = commands
            .iter()
            .find(|command| command.name == "onboarding")
            .expect("onboarding admin command must be discoverable");
        assert_eq!(command.default_member_permissions.as_deref(), Some("32"));
        let names: Vec<_> = command
            .options
            .iter()
            .map(|option| option.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "join-roles",
                "member-role",
                "clear-join",
                "clear-member",
                "panel"
            ]
        );
        let join = &command.options[0].options[0];
        assert_eq!(join.max_length, Some(209));
        assert_eq!(join.required, Some(true));
    }

    #[test]
    fn corrupt_raw_rows_disable_instead_of_laundering_or_using_deployment() {
        let deployment = HashMap::from([
            (JOIN_ROLES_KEY.into(), "1001".into()),
            (MEMBER_ROLE_KEY.into(), "1008".into()),
        ]);
        for corrupt in [
            json!([1001]),
            json!("1001"),
            json!({}),
            json!(["01"]),
            json!(["1001", "1001"]),
        ] {
            let config = RoleConfig::load("22", &deployment, Some(&corrupt), Some(&json!(1008)));
            assert!(config.invalid_join);
            assert!(config.invalid_member);
            assert!(config.join.is_empty());
            assert_eq!(config.member, None);
        }
        let empty = RoleConfig::load("22", &deployment, Some(&json!([])), None);
        assert!(empty.join.is_empty());
        assert!(!empty.invalid_join);
        assert_eq!(empty.member.as_deref(), Some("1008"));
    }
}

use std::collections::{HashMap, HashSet};

use serde_json::Value;

pub const JOIN_ROLES_KEY: &str = "TWO_ONBOARDING_JOIN_ROLES";
pub const MEMBER_ROLE_KEY: &str = "TWO_ONBOARDING_MEMBER_ROLE";
pub const MAX_JOIN_ROLES: usize = 10;
pub const VERIFY_CUSTOM_ID: &str = "two:onboarding:verify";

/// Admin-only, always discoverable; no configuration is enabled by publication.
#[must_use]
pub fn admin_command() -> crate::commands::CommandDefinition {
    use crate::commands::{
        CommandDefinition, CommandOption, CommandOptionType as Kind, PERM_MANAGE_GUILD,
    };
    CommandDefinition::new(
        "onboarding",
        "Configure join roles and the member verification button.",
    )
    .permissions(PERM_MANAGE_GUILD)
    .options(vec![
        CommandOption::new(
            "join-roles",
            "Replace the auto-role list (up to 10 role ids).",
            Kind::SubCommand,
        )
        .sub_options(vec![CommandOption::new(
            "roles",
            "Comma-separated role ids; no spaces or duplicates.",
            Kind::String,
        )
        .required()
        .max_length(209)]),
        CommandOption::new(
            "member-role",
            "Set the role granted by the Verify button.",
            Kind::SubCommand,
        )
        .sub_options(vec![CommandOption::new(
            "role",
            "Member role to grant after verification.",
            Kind::Role,
        )
        .required()]),
        CommandOption::new(
            "clear-join",
            "Clear the stored join-role override (deployment fallback applies).",
            Kind::SubCommand,
        ),
        CommandOption::new(
            "clear-member",
            "Clear the stored member-role override (deployment fallback applies).",
            Kind::SubCommand,
        ),
        CommandOption::new(
            "panel",
            "Post a Verify button in this channel.",
            Kind::SubCommand,
        ),
    ])
}

/// Canonical Discord role id, excluding the guild's everyone role.
#[must_use]
pub fn valid_role_id(guild: &str, id: &str) -> bool {
    id != guild
        && id
            .parse::<u64>()
            .is_ok_and(|number| number != 0 && number.to_string() == id)
}

/// Validate the original JSON, before env rendering can erase its types.
#[must_use]
pub fn valid_setting(guild: &str, key: &str, value: &Value) -> bool {
    match key {
        JOIN_ROLES_KEY => value.as_array().is_some_and(|roles| {
            let mut seen = HashSet::new();
            roles.len() <= MAX_JOIN_ROLES
                && roles.iter().all(|role| {
                    role.as_str()
                        .is_some_and(|id| valid_role_id(guild, id) && seen.insert(id))
                })
        }),
        MEMBER_ROLE_KEY => value.as_str().is_some_and(|id| valid_role_id(guild, id)),
        _ => true,
    }
}

#[derive(Debug, Default)]
pub struct RoleConfig {
    pub join: Vec<String>,
    pub member: Option<String>,
    pub invalid_join: bool,
    pub invalid_member: bool,
}

impl RoleConfig {
    /// Stored values win even when corrupt: never fall back to a live env grant.
    /// Invalid values disable only their feature, not the welcome worker.
    #[must_use]
    pub fn load(
        guild: &str,
        deployment: &HashMap<String, String>,
        stored_join: Option<&Value>,
        stored_member: Option<&Value>,
    ) -> Self {
        let join = stored_join.cloned().unwrap_or_else(|| {
            Value::Array(
                deployment
                    .get(JOIN_ROLES_KEY)
                    .filter(|raw| !raw.is_empty())
                    .map(|raw| {
                        raw.split(',')
                            .map(|id| Value::String(id.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        });
        let member = stored_member.cloned().or_else(|| {
            deployment
                .get(MEMBER_ROLE_KEY)
                .filter(|raw| !raw.is_empty())
                .map(|raw| Value::String(raw.clone()))
        });
        let invalid_join = !valid_setting(guild, JOIN_ROLES_KEY, &join);
        let invalid_member = member
            .as_ref()
            .is_some_and(|value| !valid_setting(guild, MEMBER_ROLE_KEY, value));
        Self {
            join: if invalid_join {
                Vec::new()
            } else {
                join.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            },
            member: member
                .filter(|_| !invalid_member)
                .and_then(|value| value.as_str().map(str::to_owned)),
            invalid_join,
            invalid_member,
        }
    }
}
