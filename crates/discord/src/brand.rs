//! Owner brand constants for user-visible Discord surfaces.
//!
//! Icon 1 (gradient "T" in a broken octagon) is the owner's chosen mark.
//! The raster asset lives at `assets/brand/two-icon-1-dark-512.png` and is
//! served from this repository's stable public URL, so every embed footer
//! icon and the bot avatar resolve to that one file through
//! [`BRAND_ICON_URL`]. Embed colour is the brand pink-red 500, the gradient
//! mid-stop (`--two-pink-500` in the brand tokens).

use twilight_model::channel::message::embed::{Embed, EmbedFooter};

/// Brand pink-red 500, the gradient mid-stop. Embed colour everywhere.
pub const BRAND_EMBED_COLOR: u32 = 0xFF0E52;

/// Stable public URL of the Icon 1 dark-background PNG (512x512).
///
/// Points at this repository so the icon stays reachable without depending
/// on the web-site card landing first; move it to the site asset URL once
/// that exists and update this one constant.
pub const BRAND_ICON_URL: &str = "https://raw.githubusercontent.com/TogetherWeOwn/two-bot-next/main/assets/brand/two-icon-1-dark-512.png";

/// Footer text shown on bot-authored embeds.
pub const BRAND_FOOTER_TEXT: &str = "Together We Own";

/// Footer block shared by every bot-authored embed.
#[must_use]
pub fn brand_footer() -> EmbedFooter {
    EmbedFooter {
        icon_url: Some(BRAND_ICON_URL.to_owned()),
        proxy_icon_url: None,
        text: BRAND_FOOTER_TEXT.to_owned(),
    }
}

/// Apply brand colour and footer to an embed under construction.
#[must_use]
pub fn brand_embed(mut embed: Embed) -> Embed {
    embed.color = Some(BRAND_EMBED_COLOR);
    embed.footer = Some(brand_footer());
    embed
}

#[cfg(test)]
mod brand_tests {
    use super::*;

    #[test]
    fn embed_color_matches_the_brand_pink_red_stop() {
        assert_eq!(BRAND_EMBED_COLOR, 0xFF0E52);
    }

    #[test]
    fn icon_url_is_a_stable_same_repo_public_url() {
        assert!(
            BRAND_ICON_URL.starts_with("https://raw.githubusercontent.com/TogetherWeOwn/"),
            "icon must resolve without depending on another card's deploy"
        );
        assert!(
            BRAND_ICON_URL.ends_with("assets/brand/two-icon-1-dark-512.png"),
            "icon must be the committed dark-background PNG"
        );
    }

    #[test]
    fn footer_text_is_short_and_branded() {
        assert!(!BRAND_FOOTER_TEXT.is_empty());
        assert!(BRAND_FOOTER_TEXT.chars().count() <= 64);
        assert_eq!(brand_footer().text, BRAND_FOOTER_TEXT);
        assert_eq!(brand_footer().icon_url.as_deref(), Some(BRAND_ICON_URL));
    }

    #[test]
    fn brand_embed_sets_color_and_footer() {
        let bare = Embed {
            author: None,
            color: None,
            description: Some("help".to_owned()),
            fields: Vec::new(),
            footer: None,
            image: None,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: None,
            url: None,
            video: None,
        };
        let branded = brand_embed(bare);
        assert_eq!(branded.color, Some(BRAND_EMBED_COLOR));
        let footer = branded.footer.expect("branded embed has a footer");
        assert_eq!(footer.text, BRAND_FOOTER_TEXT);
        assert_eq!(footer.icon_url.as_deref(), Some(BRAND_ICON_URL));
        assert_eq!(
            branded.description.as_deref(),
            Some("help"),
            "branding must not touch the body"
        );
    }
}
