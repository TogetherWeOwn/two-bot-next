//! Strict, pure decoding of `/giveaway` management options.
//!
//! Every management subcommand is decoded here, before the runtime touches the
//! store, the budget, the audit log, the draw engine or Discord. The decoder
//! never picks a winner among duplicate or competing options and never fills a
//! default after malformed input: any ambiguity becomes one [`Refusal`].
//!
//! Refusal text is assembled only from static strings. It names the supported
//! option and a repair action, and never echoes a submitted value or option
//! name, so a prize, role or credential-shaped input cannot leak back out.

use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use two_bot_core::giveaway;

/// Longest saved giveaway id, in bytes.
const ID_MAX_LEN: usize = 64;
/// Accepted spans for the minute-valued duration options.
const MINUTES_MIN: i64 = 1;
const MINUTES_MAX: i64 = 43_200;
/// Accepted span for the account and server age requirements, in days.
const AGE_MAX_DAYS: i64 = 3_650;

/// Supported subcommands, in the order the refusal lists them.
const SUBCOMMANDS: &str = "create, list, inspect, cancel, end, pause, resume, extend or reroll";

/// A validated management command, ready for the runtime to execute.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Management {
    Create(Create),
    List,
    Inspect { id: String },
    Cancel { id: String },
    End { id: String },
    Pause { id: String },
    Resume { id: String },
    Extend { id: String, seconds: i64 },
    Reroll { id: String, target: Option<String> },
}

/// Validated `create` input. The optional `channel` compatibility option is
/// checked and dropped: creation always posts in the invoking channel.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Create {
    pub(super) prize: String,
    pub(super) winners: i64,
    pub(super) seconds: i64,
    pub(super) roles: Vec<String>,
    pub(super) account_days: i64,
    pub(super) server_days: i64,
    pub(super) preview: bool,
}

/// Why a payload was refused. Rendered by [`Refusal::text`].
#[derive(Debug, PartialEq, Eq)]
enum Problem {
    Shape,
    Unknown(&'static str),
    Missing,
    Repeated,
    Type(&'static str),
    Range(&'static str),
    Id,
}

/// One private, mention-safe refusal naming the option to repair.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Refusal {
    option: &'static str,
    problem: Problem,
}

impl Refusal {
    fn new(option: &'static str, problem: Problem) -> Self {
        Self { option, problem }
    }

    pub(super) fn text(&self) -> String {
        let option = self.option;
        match &self.problem {
            Problem::Shape => format!(
                "Invalid giveaway command. Use `/giveaway` with exactly one subcommand: {SUBCOMMANDS}."
            ),
            Problem::Unknown(accepted) => format!(
                "Invalid giveaway options. Remove unsupported options from `/giveaway {option}`; {accepted}."
            ),
            Problem::Missing => {
                format!("Invalid giveaway options. Provide the required `{option}` option.")
            }
            Problem::Repeated if option == "duration" => "Invalid giveaway options. Use exactly one `duration` or `duration-minutes` option, once; remove the repeated or competing one.".to_owned(),
            Problem::Repeated => format!(
                "Invalid giveaway options. Supply `{option}` once; remove the repeated option."
            ),
            Problem::Type(kind) => {
                format!("Invalid giveaway options. Use `{option}` as {kind}.")
            }
            Problem::Range(span) => {
                format!("Invalid giveaway options. Use a `{option}` value {span}.")
            }
            Problem::Id => "Invalid giveaway options. Use an `id` of 1–64 letters, digits, `_` or `-`; copy it from `/giveaway list`.".to_owned(),
        }
    }
}

type Found<'a> = Vec<(&'static str, &'a CommandOptionValue)>;

/// Decode the single management subcommand of one `/giveaway` invocation.
pub(super) fn decode(options: &[CommandDataOption]) -> Result<Management, Refusal> {
    let shape = || Refusal::new("giveaway", Problem::Shape);
    let [sub] = options else {
        return Err(shape());
    };
    let CommandOptionValue::SubCommand(children) = &sub.value else {
        return Err(shape());
    };
    match sub.name.as_str() {
        "create" => create(children).map(Management::Create),
        "list" => {
            collect("list", children, &[])?;
            Ok(Management::List)
        }
        "inspect" => id_only("inspect", children).map(|id| Management::Inspect { id }),
        "cancel" => id_only("cancel", children).map(|id| Management::Cancel { id }),
        "end" => id_only("end", children).map(|id| Management::End { id }),
        "pause" => id_only("pause", children).map(|id| Management::Pause { id }),
        "resume" => id_only("resume", children).map(|id| Management::Resume { id }),
        "extend" => {
            let found = collect("extend", children, &["id", "duration", "duration-minutes"])?;
            let id = id(&found)?;
            let minutes = minutes(&found)?;
            Ok(Management::Extend {
                id,
                seconds: minutes * 60,
            })
        }
        "reroll" => {
            let found = collect("reroll", children, &["id", "user"])?;
            let id = id(&found)?;
            let target = match find(&found, "user") {
                None => None,
                Some(CommandOptionValue::User(user)) => Some(user.get().to_string()),
                Some(_) => return Err(Refusal::new("user", Problem::Type("a single user"))),
            };
            Ok(Management::Reroll { id, target })
        }
        _ => Err(shape()),
    }
}

fn id_only(sub: &'static str, children: &[CommandDataOption]) -> Result<String, Refusal> {
    id(&collect(sub, children, &["id"])?)
}

fn create(children: &[CommandDataOption]) -> Result<Create, Refusal> {
    let found = collect(
        "create",
        children,
        &[
            "prize",
            "duration",
            "duration-minutes",
            "winners",
            "required-role",
            "min-account-age-days",
            "min-server-age-days",
            "channel",
            "preview",
        ],
    )?;
    let prize = match find(&found, "prize") {
        None => return Err(Refusal::new("prize", Problem::Missing)),
        Some(CommandOptionValue::String(prize)) => giveaway::validate_prize(prize)
            .map_err(|_| Refusal::new("prize", Problem::Range("of 1–200 characters")))?,
        Some(_) => return Err(Refusal::new("prize", Problem::Type("text"))),
    };
    let minutes = minutes(&found)?;
    let winners = match find(&found, "winners") {
        None => return Err(Refusal::new("winners", Problem::Missing)),
        Some(CommandOptionValue::Integer(n)) => giveaway::validate_winners_count(*n)
            .map_err(|_| Refusal::new("winners", Problem::Range("between 1 and 100")))?,
        Some(_) => return Err(Refusal::new("winners", Problem::Type("a whole number"))),
    };
    let roles = match find(&found, "required-role") {
        None => Vec::new(),
        Some(CommandOptionValue::Role(role)) => vec![role.get().to_string()],
        Some(_) => return Err(Refusal::new("required-role", Problem::Type("a role"))),
    };
    let account_days = age(&found, "min-account-age-days")?;
    let server_days = age(&found, "min-server-age-days")?;
    if !matches!(
        find(&found, "channel"),
        None | Some(CommandOptionValue::Channel(_))
    ) {
        return Err(Refusal::new("channel", Problem::Type("a channel")));
    }
    let preview = match find(&found, "preview") {
        None => false,
        Some(CommandOptionValue::Boolean(value)) => *value,
        Some(_) => return Err(Refusal::new("preview", Problem::Type("a boolean"))),
    };
    Ok(Create {
        prize,
        winners,
        seconds: minutes * 60,
        roles,
        account_days,
        server_days,
        preview,
    })
}

/// Keep only supported options, each at most once. `duration` and
/// `duration-minutes` are one option under two names, so together they also
/// count at most once. The first unsupported or repeated option refuses the
/// whole payload.
fn collect<'a>(
    sub: &'static str,
    children: &'a [CommandDataOption],
    accepted: &'static [&'static str],
) -> Result<Found<'a>, Refusal> {
    let mut found: Found<'a> = Vec::new();
    for child in children {
        let Some(name) = accepted.iter().copied().find(|name| *name == child.name) else {
            return Err(Refusal::new(sub, Problem::Unknown(accepted_list(sub))));
        };
        let key = if name == "duration-minutes" {
            "duration"
        } else {
            name
        };
        if found.iter().any(|(seen, _)| *seen == key) {
            return Err(Refusal::new(key, Problem::Repeated));
        }
        found.push((key, &child.value));
    }
    Ok(found)
}

fn accepted_list(sub: &str) -> &'static str {
    match sub {
        "create" => "it takes `prize`, `duration` (or `duration-minutes`), `winners`, `required-role`, `min-account-age-days`, `min-server-age-days` and `preview`",
        "extend" => "it takes `id` and `duration` (or `duration-minutes`)",
        "reroll" => "it takes `id` and `user`",
        "list" => "it takes no options",
        _ => "it takes only `id`",
    }
}

fn find<'a>(found: &Found<'a>, name: &str) -> Option<&'a CommandOptionValue> {
    found
        .iter()
        .find_map(|(key, value)| (*key == name).then_some(*value))
}

fn id(found: &Found<'_>) -> Result<String, Refusal> {
    match find(found, "id") {
        None => Err(Refusal::new("id", Problem::Missing)),
        Some(CommandOptionValue::String(id)) if valid_id(id) => Ok(id.clone()),
        Some(CommandOptionValue::String(_)) => Err(Refusal::new("id", Problem::Id)),
        Some(_) => Err(Refusal::new("id", Problem::Type("text"))),
    }
}

fn minutes(found: &Found<'_>) -> Result<i64, Refusal> {
    match find(found, "duration") {
        None => Err(Refusal::new("duration", Problem::Missing)),
        Some(CommandOptionValue::Integer(n)) if (MINUTES_MIN..=MINUTES_MAX).contains(n) => Ok(*n),
        Some(CommandOptionValue::Integer(_)) => Err(Refusal::new(
            "duration",
            Problem::Range("between 1 and 43200 minutes"),
        )),
        Some(_) => Err(Refusal::new(
            "duration",
            Problem::Type("a whole number of minutes"),
        )),
    }
}

fn age(found: &Found<'_>, name: &'static str) -> Result<i64, Refusal> {
    match find(found, name) {
        None => Ok(0),
        Some(CommandOptionValue::Integer(n)) if (0..=AGE_MAX_DAYS).contains(n) => Ok(*n),
        Some(CommandOptionValue::Integer(_)) => Err(Refusal::new(
            name,
            Problem::Range("between 0 and 3650 days"),
        )),
        Some(_) => Err(Refusal::new(name, Problem::Type("a whole number of days"))),
    }
}

/// Saved giveaway ids: 1–64 ASCII letters, digits, `_` or `-`.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(name: &str, value: CommandOptionValue) -> CommandDataOption {
        CommandDataOption {
            name: name.into(),
            value,
        }
    }

    fn sub(name: &str, children: Vec<CommandDataOption>) -> Vec<CommandDataOption> {
        vec![opt(name, CommandOptionValue::SubCommand(children))]
    }

    fn text(value: &str) -> CommandOptionValue {
        CommandOptionValue::String(value.into())
    }

    fn int(value: i64) -> CommandOptionValue {
        CommandOptionValue::Integer(value)
    }

    #[test]
    fn each_duration_spelling_is_accepted_alone_on_extend() {
        for alias in ["duration", "duration-minutes"] {
            let options = sub("extend", vec![opt("id", text("g1")), opt(alias, int(5))]);
            assert_eq!(
                decode(&options),
                Ok(Management::Extend {
                    id: "g1".into(),
                    seconds: 300
                })
            );
        }
    }

    #[test]
    fn both_duration_spellings_never_pick_a_winner() {
        let options = sub(
            "extend",
            vec![
                opt("id", text("g1")),
                opt("duration", int(5)),
                opt("duration-minutes", int(5)),
            ],
        );
        let refusal = decode(&options).expect_err("competing aliases");
        assert!(refusal.text().contains("`duration`"));
    }

    #[test]
    fn refusals_never_echo_submitted_values_or_option_names() {
        let sentinel = "synthetic-sentinel @everyone";
        let options = sub(
            "cancel",
            vec![opt("id", text("g1")), opt(sentinel, text(sentinel))],
        );
        let refusal = decode(&options).expect_err("unknown option");
        assert!(!refusal.text().contains("synthetic"));
        assert!(!refusal.text().contains("@everyone"));
    }

    #[test]
    fn list_takes_no_options() {
        assert_eq!(decode(&sub("list", vec![])), Ok(Management::List));
        let refusal = decode(&sub("list", vec![opt("id", text("g1"))])).expect_err("stray option");
        assert!(refusal.text().contains("giveaway"));
    }

    #[test]
    fn shape_refusals_cover_missing_extra_and_unknown_subcommands() {
        assert!(decode(&[]).is_err());
        assert!(decode(&sub("nope", vec![])).is_err());
        let mut two = sub("list", vec![]);
        two.extend(sub("list", vec![]));
        assert!(decode(&two).is_err());
        assert!(decode(&[opt("list", text("x"))]).is_err());
    }
}
