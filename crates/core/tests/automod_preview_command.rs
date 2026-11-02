//! Slices 2+4: `/automod-preview` command definition + permission row,
//! staging-scoped publication.
//!
//! The definition is pinned here; since slice 4 it reaches `publish_set`
//! only for the staging guild's registry (spec §5.0, T18). Routed and
//! adjudicated everywhere, reserved against custom rows in every guild.
//! Covers spec §3.1 (options), §4.1 (ManageGuild, router-enforced) and the
//! T8 permission-matrix shape. Offline only.

use two_bot_core::automod_preview::{
    automod_preview_definition, preview_publishable_in, AUTOMOD_PREVIEW_ATTACHMENTS_MAX_UTF16,
    AUTOMOD_PREVIEW_MENTIONS_MAX, AUTOMOD_PREVIEW_TEXT_MAX_UTF16,
};
use two_bot_core::command_permissions::command_permission;
use two_bot_core::commands::PERM_MANAGE_GUILD;
use two_bot_core::{
    CustomCommand, HandlerId, InteractionRouter, RouterGates, RouterRefusal, SlashContext,
    SlashOutcome,
};

const GUILD: u64 = 2222;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn ctx(name: &str, guild_id: Option<u64>, perms: Option<u64>) -> SlashContext<'_> {
    SlashContext {
        name,
        guild_id,
        actor_permissions: perms,
        custom_row: None,
    }
}

#[test]
fn definition_has_the_spec_option_shape() {
    let def = automod_preview_definition();
    assert_eq!(def.name, "automod-preview");
    assert!(!def.description.is_empty());
    assert!(
        def.description.contains("do not paste"),
        "help warns against pasting member messages: {}",
        def.description
    );
    assert!(
        def.description.contains("personal data"),
        "help keeps the spec privacy phrase: {}",
        def.description
    );
    assert!(
        def.description
            .to_lowercase()
            .contains("smallest synthetic"),
        "help keeps the smallest-phrase guidance: {}",
        def.description
    );
    assert!(
        def.description.len() <= 100,
        "Discord caps command descriptions at 100 chars; the publish slice ships this text: {}",
        def.description
    );
    assert_eq!(
        def.default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string()),
        "picker default hides from non-admins (ManageGuild 32)"
    );
    assert!(!def.dm_permission, "guild-only: no DM evaluation");
    assert_eq!(def.options.len(), 3);
    let text = def.options.iter().find(|o| o.name == "text").unwrap();
    assert_eq!(text.kind, 3, "text is a string option");
    assert_eq!(text.required, Some(true));
    assert_eq!(text.min_length, Some(1));
    assert_eq!(text.max_length, Some(AUTOMOD_PREVIEW_TEXT_MAX_UTF16));
    assert_eq!(AUTOMOD_PREVIEW_TEXT_MAX_UTF16, 2000);
    let mentions = def.options.iter().find(|o| o.name == "mentions").unwrap();
    assert_eq!(mentions.kind, 4, "mentions is an integer option");
    assert_eq!(mentions.required, None, "mentions defaults to 0");
    assert_eq!(mentions.min_value, Some(0));
    assert_eq!(mentions.max_value, Some(AUTOMOD_PREVIEW_MENTIONS_MAX));
    assert_eq!(AUTOMOD_PREVIEW_MENTIONS_MAX, 100);
    let attachments = def
        .options
        .iter()
        .find(|o| o.name == "attachments")
        .unwrap();
    assert_eq!(attachments.kind, 3, "attachments is a string option");
    assert_eq!(attachments.required, None);
    assert_eq!(
        attachments.max_length,
        Some(AUTOMOD_PREVIEW_ATTACHMENTS_MAX_UTF16)
    );
    assert_eq!(AUTOMOD_PREVIEW_ATTACHMENTS_MAX_UTF16, 1000);
}

#[test]
fn permission_row_requires_manage_guild() {
    let row = command_permission("automod-preview").expect("permission row exists");
    assert_eq!(row.parity_row, 51);
    assert_eq!(row.required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(row.required_permissions, 32);
    assert!(row.allows(Some(PERM_MANAGE_GUILD)));
    assert!(row.allows(Some(u64::MAX)));
    assert!(!row.allows(Some(0)));
    assert!(!row.allows(None));
}

#[test]
fn routes_for_manage_guild_and_refuses_without() {
    let router = InteractionRouter::new(all_on());
    for perms in [Some(PERM_MANAGE_GUILD), Some(u64::MAX)] {
        assert_eq!(
            router.route_slash(&ctx("automod-preview", Some(GUILD), perms)),
            SlashOutcome::Handled {
                handler: HandlerId::AutomodPreview
            },
            "ManageGuild bits {perms:?} reach the handler"
        );
    }
    for perms in [Some(0), Some(8192), Some(1_099_511_627_776), None] {
        assert_eq!(
            router.route_slash(&ctx("automod-preview", Some(GUILD), perms)),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::AutomodPreviewPermission
            },
            "moderator-level bits {perms:?} get the Manage Server refusal"
        );
    }
    assert_eq!(
        RouterRefusal::AutomodPreviewPermission.message(),
        "You need the Manage Server permission to use /automod-preview. Ask a server admin to grant it.",
    );
}

#[test]
fn publishes_only_into_the_staging_guild_registry() {
    use two_bot_core::automod_runtime::{LIVE_GUILD_ID, STAGING_GUILD_ID};

    let staging: u64 = STAGING_GUILD_ID.parse().expect("staging id parses");
    let live: u64 = LIVE_GUILD_ID.parse().expect("live id parses");
    assert!(preview_publishable_in(Some(staging)));
    assert!(!preview_publishable_in(Some(live)));
    assert!(!preview_publishable_in(Some(GUILD)));
    assert!(!preview_publishable_in(None));

    for (guild, present) in [
        (Some(staging), true),
        (Some(live), false),
        (Some(GUILD), false),
        (None, false),
    ] {
        let mut gates = all_on();
        gates.configured_guild = guild;
        let set = InteractionRouter::new(gates)
            .publish_set(&[])
            .expect("set assembles");
        assert_eq!(
            set.iter().any(|def| def.name == "automod-preview"),
            present,
            "staging-only publication for configured guild {guild:?}"
        );
    }
    // Moderation off in staging: no moderation set, no preview either.
    let mut gates = all_on();
    gates.configured_guild = Some(staging);
    gates.moderation = false;
    let set = InteractionRouter::new(gates)
        .publish_set(&[])
        .expect("set assembles");
    assert!(
        !set.iter().any(|def| def.name == "automod-preview"),
        "no preview without the moderation set"
    );

    // Position: last of the moderation set, ahead of any voice names.
    let mut gates = all_on();
    gates.configured_guild = Some(staging);
    let set = InteractionRouter::new(gates)
        .publish_set(&[])
        .expect("set assembles");
    let names: Vec<_> = set.iter().map(|def| def.name.as_str()).collect();
    let preview = names
        .iter()
        .position(|name| *name == "automod-preview")
        .expect("staging set carries the preview");
    let recent = names
        .iter()
        .position(|name| *name == "moderation-recent")
        .expect("moderation set intact");
    assert_eq!(
        preview,
        recent + 1,
        "preview is the last moderation row: {names:?}"
    );
    let def = &set[preview];
    assert_eq!(def, &automod_preview_definition());
}

#[test]
fn stays_dark_outside_the_staging_guild() {
    let router = InteractionRouter::new(all_on());
    let set = router.publish_set(&[]).expect("set assembles");
    assert!(
        !set.iter().any(|def| def.name == "automod-preview"),
        "no published definition outside staging"
    );
    let row = CustomCommand {
        name: "automod-preview".to_owned(),
        description: "shadow".to_owned(),
        enabled: true,
    };
    let set = router
        .publish_set(std::slice::from_ref(&row))
        .expect("set assembles");
    assert!(
        !set.iter().any(|def| def.description == "shadow"),
        "a custom row never publishes under the dark name"
    );
    let outcome = router.route_slash(&SlashContext {
        custom_row: Some(true),
        ..ctx("automod-preview", Some(GUILD), Some(u64::MAX))
    });
    assert_eq!(
        outcome,
        SlashOutcome::Handled {
            handler: HandlerId::AutomodPreview
        },
        "dispatch shadows the custom row while dark"
    );
    for member_present in [true, false] {
        let wired = InteractionRouter::production_router(all_on(), member_present);
        let SlashOutcome::Handled { handler } =
            wired.route_slash(&ctx("automod-preview", Some(GUILD), Some(u64::MAX)))
        else {
            panic!("automod-preview must route");
        };
        assert!(
            wired.handler_for(&handler).is_some(),
            "production wiring registers the handler (member_present={member_present})"
        );
    }
}
