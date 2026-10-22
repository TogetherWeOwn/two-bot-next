//! Private, immediate birthday responses. The router owns the config permission
//! check; this decoder independently refuses unknown/duplicate options and years.

use sqlx::PgPool;
use time::{Date, Month};
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use two_bot_discord::ActionExecutor;

use crate::{birthday_store as store, jobs::ErrorClass};

pub(crate) enum Input {
    Set(i16, i16),
    Show,
    Remove,
    Configure(store::Settings),
}

pub(crate) fn decode(interaction: &Interaction) -> Option<Input> {
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return None;
    };
    let [sub] = data.options.as_slice() else {
        return None;
    };
    let CommandOptionValue::SubCommand(options) = &sub.value else {
        return None;
    };
    let mut names = std::collections::HashSet::new();
    if options.iter().any(|option| !names.insert(&option.name)) {
        return None;
    }
    match sub.name.as_str() {
        "show" if options.is_empty() => Some(Input::Show),
        "remove" if options.is_empty() => Some(Input::Remove),
        "set" if options.len() == 2 => {
            let integer = |name| {
                options.iter().find_map(|o| match &o.value {
                    CommandOptionValue::Integer(n) if o.name == name => i16::try_from(*n).ok(),
                    _ => None,
                })
            };
            let month = integer("month")?;
            let day = integer("day")?;
            let m = Month::try_from(u8::try_from(month).ok()?).ok()?;
            Date::from_calendar_date(2000, m, u8::try_from(day).ok()?).ok()?;
            Some(Input::Set(month, day))
        }
        "config" => {
            let mut settings = store::Settings::default();
            for CommandDataOption { name, value } in options {
                match (name.as_str(), value) {
                    ("channel", CommandOptionValue::Channel(id)) => {
                        settings.channel_id = Some(id.to_string())
                    }
                    ("role", CommandOptionValue::Role(id)) => {
                        settings.role_id = Some(id.to_string())
                    }
                    ("cheer", CommandOptionValue::String(text)) if valid_template(text) => {
                        settings.cheer = Some(text.clone())
                    }
                    _ => return None,
                }
            }
            Some(Input::Configure(settings))
        }
        _ => None,
    }
}

pub(crate) fn valid_template(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= 200
        && !text.chars().any(char::is_control)
        && !["@everyone", "@here", "<@"]
            .iter()
            .any(|bad| text.contains(bad))
}

pub(crate) fn render(template: Option<&str>, member: &str) -> String {
    let template = template
        .filter(|s| valid_template(s))
        .unwrap_or("Happy birthday, {user}! 🎂");
    let mention = format!("<@{member}>");
    let rendered = if template.contains("{user}") {
        template.replace("{user}", &mention)
    } else {
        format!("{template} {mention}")
    };
    if rendered.chars().count() <= 200 {
        return rendered;
    }
    let prefix: String = template
        .replace("{user}", "")
        .chars()
        .take(199 - mention.chars().count())
        .collect();
    format!("{prefix} {mention}")
}

fn display_date(month: i16, day: i16) -> String {
    let name = u8::try_from(month)
        .ok()
        .and_then(|m| Month::try_from(m).ok());
    match name {
        Some(name) => format!("{name} {day}"),
        None => "not set".to_owned(),
    }
}

/// A role is cosmetic only if it is guild-local, unmanaged, has no permissions
/// and no channel access grants. Re-check at sweep time: admin edits can change
/// what an existing role means after configuration was accepted.
pub(crate) async fn safe_role(
    rest: &ActionExecutor,
    guild: &str,
    role: &str,
) -> Result<bool, ErrorClass> {
    if role == guild {
        return Ok(false);
    }
    let roles = rest
        .get_json_checked(&format!("/guilds/{guild}/roles"))
        .await
        .map_err(|_| ErrorClass::Rest)?;
    let roles = roles
        .as_ref()
        .and_then(|v| v.as_array())
        .ok_or(ErrorClass::Rest)?;
    let Some(role) = roles.iter().find(|r| r["id"].as_str() == Some(role)) else {
        return Ok(false);
    };
    if role["managed"].as_bool() != Some(false) || role["permissions"].as_str() != Some("0") {
        return Ok(false);
    }
    let id = role["id"].as_str().ok_or(ErrorClass::Rest)?;
    let channels = rest
        .get_json_checked(&format!("/guilds/{guild}/channels"))
        .await
        .map_err(|_| ErrorClass::Rest)?;
    let channels = channels
        .as_ref()
        .and_then(|v| v.as_array())
        .ok_or(ErrorClass::Rest)?;
    for channel in channels {
        let overwrites = channel["permission_overwrites"]
            .as_array()
            .ok_or(ErrorClass::Rest)?;
        for overwrite in overwrites {
            if overwrite["id"].as_str() == Some(id) && overwrite["type"].as_u64() == Some(0) {
                let allow = overwrite["allow"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or(ErrorClass::Rest)?;
                if allow != 0 {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

pub(crate) async fn safe_channel(
    rest: &ActionExecutor,
    guild: &str,
    channel: &str,
) -> Result<bool, ErrorClass> {
    let value = rest
        .get_json_checked(&format!("/channels/{channel}"))
        .await
        .map_err(|_| ErrorClass::Rest)?;
    Ok(value.is_some_and(|v| {
        v["id"].as_str() == Some(channel)
            && v["guild_id"].as_str() == Some(guild)
            && matches!(v["type"].as_u64(), Some(0 | 5))
    }))
}

pub(crate) async fn execute(
    pool: &PgPool,
    rest: &ActionExecutor,
    interaction: &Interaction,
) -> Result<String, ErrorClass> {
    let Some(input) = decode(interaction) else {
        return Ok("Invalid birthday input. Use month/day only, never a year; show/remove take no options. Cheer text must be mention-safe and at most 200 characters.".to_owned());
    };
    let guild = interaction.guild_id.ok_or(ErrorClass::Rest)?.to_string();
    let member = interaction
        .member
        .as_ref()
        .and_then(|m| m.user.as_ref())
        .ok_or(ErrorClass::Rest)?
        .id
        .to_string();
    let _guard = store::lock(pool, &guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    match input {
        Input::Set(month, day) => {
            if !store::set(pool, &guild, &member, month, day)
                .await
                .map_err(|_| ErrorClass::Database)?
            {
                return Ok(
                    "This server's birthday list is full (100 members). No birthday was saved."
                        .to_owned(),
                );
            }
            tracing::info!(guild_id = %guild, member_id = %member, "birthday.set");
            Ok(format!(
                "Birthday saved: {} (UTC). No year is stored. Use /birthday remove to delete it.",
                display_date(month, day)
            ))
        }
        Input::Show => {
            let row = store::show(pool, &guild, &member)
                .await
                .map_err(|_| ErrorClass::Database)?;
            Ok(row.map_or_else(
                || "Your birthday is not set.".to_owned(),
                |r| {
                    format!(
                        "Your birthday: {} (UTC).",
                        display_date(r.birth_month, r.birth_day)
                    )
                },
            ))
        }
        Input::Remove => {
            store::remove(pool, &guild, &member)
                .await
                .map_err(|_| ErrorClass::Database)?;
            tracing::info!(guild_id = %guild, member_id = %member, "birthday.remove");
            Ok("Birthday removed. Any birthday role is removed by the next sweep.".to_owned())
        }
        Input::Configure(settings) => {
            if let Some(channel) = &settings.channel_id {
                if !safe_channel(rest, &guild, channel).await? {
                    return Ok(
                        "Invalid birthday channel: choose a text channel in this server."
                            .to_owned(),
                    );
                }
            }
            if let Some(role) = &settings.role_id {
                if !safe_role(rest, &guild, role).await? {
                    return Ok("Invalid birthday role: choose an unmanaged cosmetic role with no permissions or channel access grants.".to_owned());
                }
            }
            store::configure(pool, &guild, &settings)
                .await
                .map_err(|_| ErrorClass::Database)?;
            Ok("Birthday configuration saved. UTC birthdays have silent cheers and an optional cosmetic role.".to_owned())
        }
    }
}
