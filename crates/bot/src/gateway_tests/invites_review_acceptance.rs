#![cfg(test)]
//! Production-boundary supplement: receipt time is not membership chronology;
//! corrupt attribution cannot poison peers; boards, names and failures are bounded.
//! Keep these expectations independent of the implementation's SQL/render helpers.
//! The two earlier acceptance modules are intentionally unchanged.

mod failures;
mod final_leave;
mod names;
mod storage;
mod support;

use two_bot_core::router::{SlashContext, SlashOutcome};
use two_bot_core::HandlerId;

use crate::command_runtime::router_with_commands;

#[test]
fn invites_routes_to_its_exact_handler_with_the_five_core_commands_published() {
    let router = router_with_commands(support::gates());
    assert_eq!(
        router.route_slash(&SlashContext {
            name: "invites",
            guild_id: Some(support::GUILD),
            actor_permissions: None,
            custom_row: None,
        }),
        SlashOutcome::Handled {
            handler: HandlerId::Invites
        },
        "a distinct but unrelated handler is not the invites execution path"
    );
    let definitions = router.publish_set(&[]).expect("core publish set");
    let mut names: Vec<_> = definitions
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["help", "invites", "leaderboard", "privacy", "rank"]);
}
