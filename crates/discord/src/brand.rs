//! Owner brand for user-visible Discord surfaces.
//!
//! Icon 1 (gradient "T" in a broken octagon) is the owner's chosen mark.
//! The raster asset lives at `assets/brand/two-icon-1-dark-512.png`; its
//! approved Cloudflare Images copy is served through [`BRAND_ICON_URL`]
//! for embed footer icons. Avatar synchronization is a separate operation.
//!
//! Colours come from the Ember Hours design kit
//! (`TogetherWeOwn/two-design-kit`), not from hand-set hex: the generated
//! file `brand_tokens.rs` is vendored byte-exact from the kit's
//! `dist/rust/brand_tokens.rs` at the pinned tag in [`BRAND_KIT_TAG`], and
//! the mapping from kit colours to embed kinds is declared once here, in
//! [`EmbedKind`]. The drift test in this module fails on any hand edit of
//! the vendored file (it checks every byte, offline); the re-vendor script
//! `scripts/vendor_brand_tokens.py --check-upstream` compares the pinned
//! tag's bytes against the committed copy (needs network); `--check` checks
//! the pinned digest offline.
//!
//! Pinned kit tag: `v0.2.0` (the tag the kit owner announced for consumers;
//! the roadmap card was written when only `v0.1.0` existed, and `v0.2.0` is
//! a superset of it).
//!
//! Kit Discord spec notes that shape this module:
//!
//! * The default embed colour is ember (`BRAND_DISCORD_EMBED_DEFAULT`), the
//!   logo's top stop and the site's action colour. Pink means *live* on the
//!   site and nowhere else, so the old pink default moved to
//!   [`EmbedKind::Live`]; every bot embed keeps the brand while the colour
//!   language matches the site. Icon 1 (the asset) and the footer text do
//!   not change; Icon 1 is delivered through Cloudflare Images.
//! * [`BRAND_ICON_URL`] and [`BRAND_FOOTER_TEXT`] stay hand-kept here: the
//!   kit spec leaves assets and copy in `brand.rs`; only colours are tokens.
//! * Per-surface footers read `Together We Own · <surface>` (spec §2); the
//!   shared [`brand_footer`] keeps the base text so this card changes no
//!   embed's visible layout, and [`brand_footer_for`] builds the suffixed
//!   form for the restyle cards.
//!
//! Update procedure (bump tag → re-vendor → drift green), in full in
//! `docs/staging-help-embed-smoke.md`: run
//! `scripts/vendor_brand_tokens.py` (optionally `--tag`), and keep the green
//! `cargo test -p two-bot-discord brand` run in the PR.

use twilight_model::channel::message::embed::{Embed, EmbedFooter};

/// Byte-exact generated tokens from `TogetherWeOwn/two-design-kit` `v0.2.0`.
#[path = "brand_tokens.rs"]
pub mod brand_tokens;

/// Pinned design-kit release tag the vendored tokens were copied from.
///
/// `scripts/vendor_brand_tokens.py --check-upstream` compares exactly this
/// tag against `brand_tokens.rs`; use `--tag` to re-vendor a new release.
pub const BRAND_KIT_TAG: &str = "v0.2.0";

/// FNV-1a 64 hash of the vendored `brand_tokens.rs` bytes at [`BRAND_KIT_TAG`].
///
/// The drift test recomputes this over `include_bytes!`, so any hand edit —
/// even one constant — fails offline. `scripts/vendor_brand_tokens.py`
/// prints the replacement value after a legitimate re-vendor.
pub const BRAND_TOKENS_FNV1A64: u64 = 0x8EBB19B4E7C8C29F;

/// Kit Discord embed colour, by meaning (kit Discord spec §3).
///
/// Later restyle cards call [`brand_embed`] with one of these instead of
/// setting colours inline; the mapping to kit constants lives only in
/// [`EmbedKind::color`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedKind {
    /// The community speaking: events posted, help, announcements.
    Default,
    /// Something is happening now.
    Live,
    /// Frozen attendance.
    Echo,
    /// A member's own action succeeded.
    Ok,
    /// A change or degraded state.
    Warn,
    /// Cancelled or failed.
    Danger,
    /// Moderator-only and diagnostics.
    Neutral,
}

impl EmbedKind {
    /// The kit colour for this kind. The generated module is the only
    /// source: no hex literal may appear here.
    #[must_use]
    pub fn color(self) -> u32 {
        use self::brand_tokens::*;
        match self {
            Self::Default => BRAND_DISCORD_EMBED_DEFAULT,
            Self::Live => BRAND_DISCORD_EMBED_LIVE,
            Self::Echo => BRAND_DISCORD_EMBED_ECHO,
            Self::Ok => BRAND_DISCORD_EMBED_OK,
            Self::Warn => BRAND_DISCORD_EMBED_WARN,
            Self::Danger => BRAND_DISCORD_EMBED_DANGER,
            Self::Neutral => BRAND_DISCORD_EMBED_NEUTRAL,
        }
    }
}

/// Public Cloudflare Images delivery URL of the approved Icon 1 dark PNG (512x512).
///
/// The committed original remains the rollback asset; see
/// `docs/staging-help-embed-smoke.md` for the delivery and rollback contract.
pub const BRAND_ICON_URL: &str =
    "https://imagedelivery.net/cNVnKlHCCMxI9vqDkwBbUQ/two-icon1-dark512-4469ca165f03/public";

/// Footer text shown on bot-authored embeds.
pub const BRAND_FOOTER_TEXT: &str = "Together We Own";

/// Footer block shared by every bot-authored embed.
#[must_use]
pub fn brand_footer() -> EmbedFooter {
    brand_footer_for("")
}

/// Footer block for one named surface, per the kit Discord spec
/// (`Together We Own · <surface>`). An empty surface keeps the base text,
/// which is what [`brand_footer`] does.
#[must_use]
pub fn brand_footer_for(surface: &str) -> EmbedFooter {
    let text = if surface.is_empty() {
        BRAND_FOOTER_TEXT.to_owned()
    } else {
        format!("{BRAND_FOOTER_TEXT} · {surface}")
    };
    EmbedFooter {
        icon_url: Some(BRAND_ICON_URL.to_owned()),
        proxy_icon_url: None,
        text,
    }
}

/// Apply one kit embed colour and the shared footer to an embed under
/// construction.
#[must_use]
pub fn brand_embed(kind: EmbedKind, mut embed: Embed) -> Embed {
    embed.color = Some(kind.color());
    embed.footer = Some(brand_footer());
    embed
}

#[cfg(test)]
mod brand_tests {
    use super::*;

    /// FNV-1a 64 over bytes, the same algorithm
    /// `scripts/vendor_brand_tokens.py` uses to print the drift value.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    #[test]
    fn fnv_detector_matches_the_official_test_vector() {
        // Proves the hash the drift test relies on is a real FNV-1a 64:
        // any byte change in the vendored file changes the digest below.
        assert_eq!(fnv1a64(b"foobar"), 0x85944171_f73967e8);
        assert_ne!(fnv1a64(b"foobar"), fnv1a64(b"foobaR"));
    }

    #[test]
    fn vendored_tokens_match_the_pinned_tag_byte_for_byte() {
        let bytes = include_bytes!("brand_tokens.rs");
        assert!(
            bytes.starts_with(b"//! DO NOT EDIT"),
            "the vendored file must stay the kit's generated output"
        );
        assert_eq!(
            fnv1a64(bytes),
            BRAND_TOKENS_FNV1A64,
            "brand_tokens.rs drifted from kit tag {BRAND_KIT_TAG}: \
             re-run scripts/vendor_brand_tokens.py instead of hand-editing"
        );
    }

    #[test]
    fn every_kind_maps_to_its_kit_constant() {
        use super::brand_tokens::*;
        assert_eq!(EmbedKind::Default.color(), BRAND_DISCORD_EMBED_DEFAULT);
        assert_eq!(EmbedKind::Live.color(), BRAND_DISCORD_EMBED_LIVE);
        assert_eq!(EmbedKind::Echo.color(), BRAND_DISCORD_EMBED_ECHO);
        assert_eq!(EmbedKind::Ok.color(), BRAND_DISCORD_EMBED_OK);
        assert_eq!(EmbedKind::Warn.color(), BRAND_DISCORD_EMBED_WARN);
        assert_eq!(EmbedKind::Danger.color(), BRAND_DISCORD_EMBED_DANGER);
        assert_eq!(EmbedKind::Neutral.color(), BRAND_DISCORD_EMBED_NEUTRAL);
    }

    #[test]
    fn default_kind_is_the_kit_ember_action_colour() {
        // The kit moved the default from pink to ember; pink now means live
        // only. Asserted through the generated module, never a hex literal.
        assert_eq!(
            EmbedKind::Default.color(),
            brand_tokens::BRAND_COLOR_EMBER,
            "the default embed colour must stay the kit ember action colour"
        );
        assert_eq!(
            EmbedKind::Live.color(),
            brand_tokens::BRAND_COLOR_STATE_LIVE,
            "live keeps the kit state-live rose"
        );
    }

    #[test]
    fn kit_tag_is_pinned_and_recorded() {
        assert!(
            BRAND_KIT_TAG.starts_with('v'),
            "the pinned tag must be a kit release tag"
        );
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
    fn surface_footer_follows_the_kit_surface_form() {
        let footer = brand_footer_for("Events");
        assert_eq!(footer.text, "Together We Own · Events");
        assert_eq!(footer.icon_url.as_deref(), Some(BRAND_ICON_URL));
        assert_eq!(
            brand_footer_for("").text,
            BRAND_FOOTER_TEXT,
            "an empty surface keeps the base footer text"
        );
    }

    #[test]
    fn brand_embed_sets_kind_color_and_footer() {
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
        for kind in [
            EmbedKind::Default,
            EmbedKind::Live,
            EmbedKind::Echo,
            EmbedKind::Ok,
            EmbedKind::Warn,
            EmbedKind::Danger,
            EmbedKind::Neutral,
        ] {
            let branded = brand_embed(kind, bare.clone());
            assert_eq!(branded.color, Some(kind.color()));
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
}
