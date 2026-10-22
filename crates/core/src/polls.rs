//! Bounds and compiled slash surface for single-question timed polls.

use crate::commands::{CommandDefinition, CommandOption, CommandOptionType};

pub const MAX_QUESTION: usize = 256;
pub const MAX_OPTION: usize = 80;
pub const MAX_OPTIONS: usize = 10;
pub const MAX_DURATION_MINUTES: i64 = 10_080;
pub const MAX_OPEN_GUILD: i64 = 10;
pub const MAX_OPEN_AUTHOR: i64 = 3;
pub const MAX_VOTERS: i64 = 1000;

/// Validated inputs retain raw text in storage, never in diagnostics.
pub struct PollInput {
    pub question: String,
    pub options: Vec<String>,
    pub minutes: i64,
}

impl PollInput {
    pub fn parse(question: &str, options: &[String], minutes: i64) -> Option<Self> {
        let question = question.trim();
        let options: Vec<String> = options.iter().map(|s| s.trim().to_owned()).collect();
        if !(1..=MAX_QUESTION).contains(&question.chars().count())
            || !(2..=MAX_OPTIONS).contains(&options.len())
            || options
                .iter()
                .any(|s| !(1..=MAX_OPTION).contains(&s.chars().count()))
            || !(1..=MAX_DURATION_MINUTES).contains(&minutes)
        {
            return None;
        }
        Some(Self {
            question: question.to_owned(),
            options,
            minutes,
        })
    }
}

pub fn command() -> CommandDefinition {
    use CommandOptionType::{Integer, String, SubCommand};
    let mut args = vec![
        CommandOption::new("question", "Single poll question", String)
            .required()
            .max_length(MAX_QUESTION as u32),
        CommandOption::new("option1", "First choice", String)
            .required()
            .max_length(MAX_OPTION as u32),
        CommandOption::new("option2", "Second choice", String)
            .required()
            .max_length(MAX_OPTION as u32),
        CommandOption::new(
            "duration-minutes",
            "Close after 1 minute to 7 days",
            Integer,
        )
        .required()
        .int_range(1, MAX_DURATION_MINUTES),
    ];
    for i in 3..=MAX_OPTIONS {
        args.push(
            CommandOption::new(&format!("option{i}"), "Additional choice", String)
                .max_length(MAX_OPTION as u32),
        );
    }
    CommandDefinition::new("poll", "Create a timed poll or close your poll").options(vec![
        CommandOption::new(
            "create",
            "One changeable vote per member; up to 10 choices",
            SubCommand,
        )
        .sub_options(args),
        CommandOption::new("close", "Close your poll and publish results", SubCommand).sub_options(
            vec![
                CommandOption::new("id", "Poll id shown in the poll message", String)
                    .required()
                    .max_length(20),
            ],
        ),
    ])
}
