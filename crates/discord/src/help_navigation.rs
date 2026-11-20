//! Render confirmed discovery pages as new, private Discord callbacks.

use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
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
const HELP_SURFACE: &str = "Help";
// Bump when page chrome, navigation labels, or control behavior changes.
const RENDER_VERSION: &str = "discovery-v1";

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

fn help_callback(
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
