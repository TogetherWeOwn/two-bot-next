//! Registration identity is separate from the ID-free command drift hash.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};
use twilight_model::application::{
    command::{Command, CommandType},
    interaction::{Interaction, InteractionData, InteractionType},
};

#[derive(Debug, Clone)]
struct RegisteredCommand {
    id: Option<u64>,
    kind: CommandType,
    name: String,
}

/// Clone-shared snapshots of confirmed guild registrations. A full overwrite
/// replaces a scope, so removed and recreated IDs cannot accumulate.
#[derive(Debug, Clone, Default)]
pub struct CommandIdentities {
    guilds: Arc<RwLock<BTreeMap<(u64, u64), Vec<RegisteredCommand>>>>,
}

impl CommandIdentities {
    /// Install one successful REST receipt atomically. Twilight also represents
    /// pre-registration definitions, so absent IDs remain explicitly unknown.
    pub fn replace_guild(
        &self,
        application_id: u64,
        guild_id: u64,
        commands: &[Command],
    ) -> Result<(), &'static str> {
        if application_id == 0 || guild_id == 0 {
            return Err("invalid command identity scope");
        }
        let mut registered: Vec<RegisteredCommand> = Vec::with_capacity(commands.len());
        for command in commands {
            if command
                .application_id
                .is_some_and(|id| id.get() != application_id)
                || command.guild_id.is_some_and(|id| id.get() != guild_id)
                || registered.iter().any(|entry| {
                    (entry.kind == command.kind && entry.name == command.name)
                        || (command.id.is_some() && entry.id == command.id.map(|id| id.get()))
                })
            {
                return Err("invalid command identity receipt");
            }
            registered.push(RegisteredCommand {
                id: command.id.map(|id| id.get()),
                kind: command.kind,
                name: command.name.clone(),
            });
        }
        self.guilds
            .write()
            .map_err(|_| "command identity lock unavailable")?
            .insert((application_id, guild_id), registered);
        Ok(())
    }

    /// Resolve slash identity before either admission or voice parsing.
    /// Discord sends the invoked command's ID and registration guild separately
    /// from the invocation guild; a global command has no registration guild.
    /// <https://docs.discord.com/developers/interactions/receiving-and-responding#application-command-data-structure>
    #[must_use]
    pub fn slash_name(&self, interaction: &Interaction) -> Option<String> {
        if interaction.kind != InteractionType::ApplicationCommand {
            return None;
        }
        let InteractionData::ApplicationCommand(command) = interaction.data.as_ref()? else {
            return None;
        };
        if command.kind != CommandType::ChatInput {
            return None;
        }
        let application = interaction.application_id.get();
        let guild = command.guild_id.map(|id| id.get());
        if guild.is_some() && guild != interaction.guild_id.map(|id| id.get()) {
            return None;
        }
        let guilds = self.guilds.read().ok()?;
        // Once publication pins an application, another application's payload
        // must not regain name fallback through an uninitialized scope.
        if !guilds.is_empty() && !guilds.keys().any(|(app, _)| *app == application) {
            return None;
        }
        let scope = guild.and_then(|guild| guilds.get(&(application, guild)));
        if let Some(registered) = scope {
            if let Some(entry) = registered
                .iter()
                .find(|entry| entry.id == Some(command.id.get()))
            {
                return (entry.kind == command.kind).then(|| entry.name.clone());
            }
            // Fallback only for a registration whose ID is genuinely unknown,
            // not an unknown/obsolete ID claiming a known voice command name.
            return registered
                .iter()
                .find(|entry| {
                    entry.kind == command.kind && entry.name == command.name && entry.id.is_none()
                })
                .map(|entry| entry.name.clone());
        }
        // A known ID cannot be replayed under a different registration scope.
        if guilds.values().any(|entries| {
            entries
                .iter()
                .any(|entry| entry.id == Some(command.id.get()))
        }) {
            return None;
        }
        Some(command.name.clone())
    }
}
