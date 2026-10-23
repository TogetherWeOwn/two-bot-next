//! Guild-scoped classifier settings shared by gateway facts and attendance.

use std::collections::HashMap;
use two_bot_core::{settings::LiveSettings, ClassifierConfig};

/// Look up the reader at execution time: runtimes may boot before the settings
/// job registers it, and ordered completion may follow a new publication.
#[derive(Debug)]
pub(crate) struct LiveClassifier;

impl two_bot_discord::rsvp::ClassifierProvider for LiveClassifier {
    fn config_for_guild(&self, guild_id: &str) -> ClassifierConfig {
        let deployment = std::env::vars().collect();
        match crate::settings_jobs::live() {
            Some(live) => classifier_config_from_live(&deployment, guild_id, &live),
            None => ClassifierConfig::from_map(&deployment),
        }
    }
}

/// Stored rows replace deployment channel/exclusion sets for one guild. Empty
/// rows clear a set; deleted rows fall back to deployment. One snapshot supplies
/// all seven keys; classifier version and other fields remain deployment-only.
pub(crate) fn classifier_config_from_live(
    deployment: &HashMap<String, String>,
    guild_id: &str,
    live: &LiveSettings,
) -> ClassifierConfig {
    let mut vars = deployment.clone();
    vars.extend(
        live.env_snapshot(Some(guild_id))
            .into_iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "TWO_COMMUNITY_HUMAN_CHANNEL_IDS"
                        | "TWO_COMMUNITY_WELCOME_CHANNEL_IDS"
                        | "TWO_COMMUNITY_AUTOMATION_ACTOR_IDS"
                        | "TWO_COMMUNITY_RAID_ACTOR_IDS"
                        | "TWO_COMMUNITY_STAGING_ACTOR_IDS"
                        | "TWO_COMMUNITY_STAGING_GUILD_IDS"
                        | "TWO_COMMUNITY_TEST_ACTOR_IDS"
                )
            }),
    );
    ClassifierConfig::from_map(&vars)
}
