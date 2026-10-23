//! Personal channel reminders share the scheduled delivery fence, not staff quotas.

use crate::commands::{CommandDefinition, CommandOption, CommandOptionType};
use crate::message_safety::{has_message_text, text_len};
use crate::scheduled::{neutralize_scheduled_body, IN_MINUTES_MAX, IN_MINUTES_MIN};

/// Small enough that all five reminders, including ids and due times, fit one reply.
pub const TEXT_LIMIT: usize = 240;
pub const PENDING_LIMIT: i64 = 5;
/// Reserved storage namespace. Staff scheduling never creates or edits these ids.
pub const ID_PREFIX: &str = "remind-";

#[must_use]
pub fn command() -> CommandDefinition {
    CommandDefinition::new("remind", "Manage your personal reminders in this channel").options(
        vec![
            CommandOption::new(
                "me",
                "Remind me here after a delay",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![
                CommandOption::new(
                    "in",
                    "Minutes from now (1 to 525600)",
                    CommandOptionType::Integer,
                )
                .required()
                .int_range(IN_MINUTES_MIN, IN_MINUTES_MAX),
                CommandOption::new(
                    "text",
                    "Reminder text (no pings)",
                    CommandOptionType::String,
                )
                .required()
                .max_length(TEXT_LIMIT as u32),
            ]),
            CommandOption::new(
                "list",
                "List your pending reminders in this server",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "cancel",
                "Cancel one of your pending reminders",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "target",
                "Full id, unique id prefix or unique full reminder text",
                CommandOptionType::String,
            )
            .required()
            .max_length(TEXT_LIMIT as u32)]),
        ],
    )
}

/// Validate before any database operation; safety expansion counts toward the limit.
pub fn validate(minutes: i64, text: &str) -> Result<String, &'static str> {
    if !(IN_MINUTES_MIN..=IN_MINUTES_MAX).contains(&minutes) {
        return Err("Reminder delay must be between 1 and 525600 minutes.");
    }
    if text_len(text) > TEXT_LIMIT {
        return Err("Reminder text must fit within 240 characters.");
    }
    let safe = neutralize_scheduled_body(text);
    if text_len(&safe) > TEXT_LIMIT {
        return Err("Reminder text must fit within 240 characters after safety formatting.");
    }
    if !has_message_text(&safe) {
        return Err("Reminder text must not be empty.");
    }
    Ok(safe)
}

#[must_use]
pub fn delivery_body(owner: &str, text: &str) -> String {
    // Literal identity, not a mention: a reminder never pings anyone.
    format!("Reminder for {owner}: {text}")
}
