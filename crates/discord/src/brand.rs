//! Owner brand constants for user-visible Discord surfaces.
//!
//! Icon 1 (gradient "T" in a broken octagon) is the owner's chosen mark.
//! The raster asset lives at `assets/brand/two-icon-1-dark-512.png`; its
//! approved Cloudflare Images copy is served through [`BRAND_ICON_URL`]
//! for embed footer icons. Avatar synchronization is a separate operation.
//! Embed colour is the brand pink-red 500, the gradient mid-stop
//! (`--two-pink-500` in the brand tokens).

use twilight_model::channel::message::embed::{Embed, EmbedFooter};

/// Brand pink-red 500, the gradient mid-stop. Embed colour everywhere.
pub const BRAND_EMBED_COLOR: u32 = 0xFF0E52;

/// Public Cloudflare Images delivery URL of the approved Icon 1 dark PNG (512x512).
///
/// The committed original remains the rollback asset; see
/// `docs/staging-help-embed-smoke.md` for the delivery and rollback contract.
pub const BRAND_ICON_URL: &str = "https://imagedelivery.net/cNVnKlHCCMxI9vqDkwBbUQ/two-icon1-dark512-4469ca165f03/public";

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
    fn icon_url_is_the_approved_cloudflare_images_delivery() {
        assert_eq!(
            BRAND_ICON_URL,
            "https://imagedelivery.net/cNVnKlHCCMxI9vqDkwBbUQ/two-icon1-dark512-4469ca165f03/public",
            "icon must use the approved Icon 1 image and public variant"
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
