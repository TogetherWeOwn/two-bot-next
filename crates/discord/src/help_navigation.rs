//! Render confirmed discovery pages as new, private Discord callbacks.

use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle, ComponentType};
use twilight_model::channel::message::embed::{Embed, EmbedField, EmbedFooter};
use twilight_model::channel::message::{Component, MessageFlags};
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use two_bot_core::help::cursor::{Cursor, CursorError, Revision};
use two_bot_core::help::pages::{Page, PlanError, MAX_HELP_PAGES};
use two_bot_core::help::plan_discovery;
use two_bot_core::message_safety::text_len;
use two_bot_core::CommandDefinition;

use crate::brand::{brand_embed, brand_footer_for, EmbedKind, BRAND_KIT_TAG};

pub const HELP_TITLE: &str = "Together We Own help";
pub(crate) const HELP_SURFACE: &str = "Help";
// Bump when page chrome, navigation labels, or control behavior changes.
const RENDER_VERSION: &str = "discovery-v1";

/// Read a cursor only from a button on the app's own private reply to this actor.
/// Call after the router admits the guild. Pass the bot and application IDs
/// from runtime identity, not from the clicked message or custom ID.
pub fn source_cursor(
    interaction: &Interaction,
    application: u64,
    bot_user: u64,
) -> Option<(Cursor, u64)> {
    let (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data))) =
        (&interaction.kind, interaction.data.as_ref())
    else {
        return None;
    };
    let guild = interaction.guild_id?;
    let actor = interaction.author_id()?;
    let message = interaction.message.as_ref()?;
    let metadata = message.interaction_metadata.as_ref()?;
    if interaction.application_id.get() != application
        || data.component_type != ComponentType::Button
        || !data.values.is_empty()
        || !message.author.bot
        || message.author.id.get() != bot_user
        || message
            .application_id
            .is_some_and(|id| id.get() != application)
        || message.guild_id.is_some_and(|id| id != guild)
        || !message
            .flags
            .is_some_and(|flags| flags.contains(MessageFlags::EPHEMERAL))
        || metadata.user.id != actor
        || metadata.original_response_message_id.is_some()
        || !matches!(
            metadata.kind,
            InteractionType::ApplicationCommand | InteractionType::MessageComponent
        )
    {
        return None;
    }
    let cursor = Cursor::parse(&data.custom_id)?;
    if !cursor.is_for_actor(actor.get()) {
        return None;
    }
    let on_source = message.components.iter().any(|component| {
        let Component::ActionRow(row) = component else {
            return false;
        };
        row.components.iter().any(|component| {
            matches!(component, Component::Button(button)
                if !button.disabled && button.custom_id.as_deref() == Some(data.custom_id.as_str()))
        })
    });
    on_source.then_some((cursor, actor.get()))
}

pub struct DiscoveryPlan {
    pages: Vec<Page>,
    revision: Revision,
}

impl DiscoveryPlan {
    /// Prepare one independently renderable plan from a confirmed publication.
    pub fn new(
        defs: &[CommandDefinition],
        guild: u64,
        application: u64,
    ) -> Result<Self, PlanError> {
        let longest_footer = page_footer(MAX_HELP_PAGES, MAX_HELP_PAGES);
        let reserved = text_len(HELP_TITLE) + text_len(&longest_footer.text);
        let pages = plan_discovery(defs, reserved)?;
        let version = format!("{RENDER_VERSION}:{BRAND_KIT_TAG}");
        let revision = Revision::for_pages(guild, application, &version, &pages);
        Ok(Self { pages, revision })
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Render page one with an expiry fixed by the original help invocation.
    pub fn first(&self, actor: u64, expires_at: u64) -> Option<InteractionResponse> {
        self.render(0, actor, expires_at)
    }

    /// Render a validated absolute target without editing its source message.
    /// Validate the Discord source and guild before calling this method.
    pub fn navigate(
        &self,
        cursor: Cursor,
        actor: u64,
        now: u64,
    ) -> Result<InteractionResponse, CursorError> {
        let (index, expires_at) = cursor.target(actor, now, self.revision, self.pages.len())?;
        self.render(index, actor, expires_at)
            .ok_or(CursorError::Invalid)
    }

    fn render(&self, index: usize, actor: u64, expires_at: u64) -> Option<InteractionResponse> {
        let page = self.pages.get(index)?;
        let count = self.pages.len();
        let mut buttons = Vec::new();
        let mut add_button = |target, style, direction| {
            let cursor = Cursor::new(self.revision, actor, expires_at, target)?;
            buttons.push(Component::Button(Button {
                id: None,
                custom_id: Some(cursor.encode()),
                disabled: false,
                emoji: None,
                label: Some(format!("{direction} to page {}", target + 1)),
                style,
                url: None,
                sku_id: None,
            }));
            Some(())
        };
        if index >= 2 {
            add_button(0, ButtonStyle::Secondary, "Back")?;
        }
        if index >= 1 {
            add_button(index - 1, ButtonStyle::Secondary, "Back")?;
        }
        if index + 1 < count {
            add_button(index + 1, ButtonStyle::Primary, "Go")?;
        }
        let components = (!buttons.is_empty()).then(|| {
            vec![Component::ActionRow(ActionRow {
                id: None,
                components: buttons,
            })]
        });
        let fields = page
            .fields
            .iter()
            .map(|(name, value)| EmbedField {
                inline: false,
                name: name.clone(),
                value: value.clone(),
            })
            .collect();
        Some(help_callback(
            EmbedKind::Default,
            HELP_TITLE,
            &page.description,
            fields,
            page_footer(index + 1, count),
            components,
        ))
    }
}

fn page_footer(page: usize, count: usize) -> EmbedFooter {
    if count == 1 {
        brand_footer_for(HELP_SURFACE)
    } else {
        brand_footer_for(&format!("{HELP_SURFACE} · Page {page} of {count}"))
    }
}

#[derive(Clone, Copy)]
pub enum HelpRecovery {
    Stale,
    Expired,
    Invalid,
    Busy { from_button: bool },
    CannotBuild,
}

impl From<CursorError> for HelpRecovery {
    fn from(error: CursorError) -> Self {
        match error {
            CursorError::Stale => Self::Stale,
            CursorError::Expired => Self::Expired,
            CursorError::Invalid => Self::Invalid,
        }
    }
}

impl HelpRecovery {
    pub fn response(self) -> InteractionResponse {
        let (kind, title, description) = match self {
            Self::Stale => (
                EmbedKind::Warn,
                "The command list changed",
                "This page is from an older list, so the bot didn't open it. Run `/help` to see the current one.",
            ),
            Self::Expired => (
                EmbedKind::Warn,
                "This help page has expired",
                "Help pages last 10 minutes. Run `/help` to start again.",
            ),
            Self::Invalid => (
                EmbedKind::Warn,
                "That help button doesn't work here",
                "Run `/help` to get your own list.",
            ),
            Self::Busy { from_button } => (
                EmbedKind::Warn,
                "The bot is catching up",
                if from_button {
                    "It couldn't open that page just now. Press the same button again in a moment."
                } else {
                    "It couldn't open help just now. Run `/help` again in a moment."
                },
            ),
            Self::CannotBuild => (
                EmbedKind::Danger,
                "Help isn't available right now",
                "The bot couldn't build the command list. Try `/help` again in a few minutes.",
            ),
        };
        help_callback(
            kind,
            title,
            description,
            Vec::new(),
            brand_footer_for(HELP_SURFACE),
            None,
        )
    }
}

pub(crate) fn help_callback(
    kind: EmbedKind,
    title: &str,
    description: &str,
    fields: Vec<EmbedField>,
    footer: EmbedFooter,
    components: Option<Vec<Component>>,
) -> InteractionResponse {
    let mut embed = brand_embed(
        kind,
        Embed {
            author: None,
            color: None,
            description: Some(description.to_owned()),
            fields,
            flags: None,
            footer: None,
            image: None,
            kind: "rich".into(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: Some(title.to_owned()),
            url: None,
            video: None,
        },
    );
    embed.footer = Some(footer);
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            allowed_mentions: Some(Default::default()),
            components,
            embeds: Some(vec![embed]),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests;
