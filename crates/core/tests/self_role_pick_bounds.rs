//! Self-role pick-bounds acceptance.
//!
//! Pins the Owen self-role bounds offline through the existing planner API
//! only (spec: `docs/parity.md` §12 — 20 reactions, 100-character IDs,
//! 80-character labels; valid-pick/canonical-order/fallback delta).
//!
//! All assertions go through the public `self_roles` and `onboarding`
//! planner APIs only. Synthetic fixtures only: no Discord, network, database,
//! or handler/store/gateway wiring (runtime wiring stays TOG-10292 for
//! self-roles and TOG-10278 for session/onboarding picks).

use std::collections::HashSet;

use two_bot_core::onboarding::{
    build_session_picks, plan_game_selection, plan_session, resolve_destination, GamePick,
};
use two_bot_core::self_roles::{
    parse_self_role_panels, plan_select_delta, plan_self_role_change, PanelMode, PlanRejection,
    RoleOperation, SelfRolePanel, SettledOutcome,
};

const GUILD: &str = "111111111111111111";
const MSG: &str = "555555555555555555";
const ROLE_A: &str = "222222222222222222";
const ROLE_B: &str = "333333333333333333";

/// Snowflake for reaction-fixture option `i` (18 digits, all numeric).
fn snowflake(i: u64) -> String {
    format!("{}", 100000000000000000u64 + i)
}

fn reaction_option(i: usize) -> serde_json::Value {
    let id = snowflake(i as u64);
    serde_json::json!({
        "key": format!("opt{i:02}"),
        "label": format!("Option {i}"),
        "roleId": snowflake(100 + i as u64),
        "permissions": "0",
        "emoji": format!("<:e{i}:{id}>"),
    })
}

fn reaction_panel_raw(count: usize) -> String {
    let options: Vec<serde_json::Value> = (0..count).map(reaction_option).collect();
    serde_json::json!([{
        "id": "react",
        "channelId": GUILD,
        "messageId": MSG,
        "mode": "reaction",
        "options": options,
    }])
    .to_string()
}

#[test]
fn reaction_panels_refuse_more_than_twenty_options() {
    // Discord caps a message at 20 reactions: exactly 20 parses.
    let panels = parse_self_role_panels(&reaction_panel_raw(20)).expect("20 parses");
    assert_eq!(panels[0].options.len(), 20);
    // The 21st pick is refused at catalogue load with the documented reason.
    let err = parse_self_role_panels(&reaction_panel_raw(21)).expect_err("21 refused");
    assert!(err.message().contains("20-reaction limit"), "{err}");
}

#[test]
fn button_custom_ids_refuse_over_one_hundred_utf16_units() {
    // "two:self-role:" (14) + 60-char panel id + ":" + 25-char key = 100.
    let id = "p".repeat(60);
    let key = "k".repeat(25);
    let raw = serde_json::json!([{
        "id": id,
        "channelId": GUILD,
        "messageId": MSG,
        "mode": "button",
        "options": [{
            "key": key, "label": "Chess",
            "roleId": ROLE_A, "permissions": "0",
        }],
    }])
    .to_string();
    assert!(parse_self_role_panels(&raw).is_ok());
    // One more key character (101 units) is refused with the documented reason.
    let raw = serde_json::json!([{
        "id": id,
        "channelId": GUILD,
        "messageId": MSG,
        "mode": "button",
        "options": [{
            "key": "k".repeat(26), "label": "Chess",
            "roleId": ROLE_A, "permissions": "0",
        }],
    }])
    .to_string();
    let err = parse_self_role_panels(&raw).expect_err("101 refused");
    assert!(
        err.message().contains("100-character custom id limit"),
        "{err}"
    );
}

#[test]
fn button_labels_refuse_over_eighty_chars() {
    let raw_with = |label: &str| {
        serde_json::json!([{
            "id": "games",
            "channelId": GUILD,
            "messageId": MSG,
            "mode": "button",
            "options": [{
                "key": "chess", "label": label,
                "roleId": ROLE_A, "permissions": "0",
            }],
        }])
        .to_string()
    };
    // ASCII and astral boundaries: "🎲" is two UTF-16 units, like legacy
    // JavaScript `string.length`.
    for label in ["a".repeat(80), "🎲".repeat(40)] {
        assert_eq!(label.encode_utf16().count(), 80);
        assert!(parse_self_role_panels(&raw_with(&label)).is_ok());
        let err = parse_self_role_panels(&raw_with(&format!("{label}a"))).expect_err("81 refused");
        assert!(err.message().contains("options[0].label"), "{err}");
    }
}

fn button_panel() -> SelfRolePanel {
    let raw = serde_json::json!([{
        "id": "games",
        "channelId": GUILD,
        "messageId": MSG,
        "mode": "button",
        "options": [
            {"key": "chess", "label": "Chess",
             "roleId": ROLE_A, "permissions": "0"},
            {"key": "go", "label": "Go",
             "roleId": ROLE_B, "permissions": "0"},
        ],
    }])
    .to_string();
    parse_self_role_panels(&raw).expect("fixture parses")[0].clone()
}

fn held(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn unknown_picks_refuse_with_documented_reason() {
    let panel = button_panel();
    // Unknown option key: stable code plus the human reason.
    let err = plan_self_role_change(&panel, "nope", &held(&[]), PanelMode::Button, false)
        .expect_err("unknown option refused");
    assert_eq!(err.code(), "unknown_option");
    assert_eq!(
        err,
        PlanRejection::UnknownOption {
            panel_id: "games".to_owned(),
            option_key: "nope".to_owned(),
        }
    );
    assert_eq!(err.reason(), "panel games has no option nope");
    // Wrong source surface: a reaction delivery never toggles a button panel.
    let err = plan_self_role_change(&panel, "chess", &held(&[]), PanelMode::Reaction, false)
        .expect_err("wrong source refused");
    assert_eq!(err.code(), "wrong_source");
    // Select delta naming a role outside the catalogue is refused, not granted.
    let err = plan_select_delta(&panel, &held(&[]), &["999999999999999999".to_owned()])
        .expect_err("off-catalogue role refused");
    assert_eq!(err.code(), "unknown_option");
}

#[test]
fn select_delta_emits_canonical_order_and_null_option_fallback() {
    let panel = button_panel();
    // Submitted [B, A], but mutations follow catalogue order [A, B]. A pure
    // grant on a non-exclusive panel is an add, not a replace.
    let desired = vec![ROLE_B.to_owned(), ROLE_A.to_owned()];
    let plan = plan_select_delta(&panel, &held(&[]), &desired).expect("plans");
    assert_eq!(plan.operation, RoleOperation::Add);
    assert_eq!(plan.add_role_ids, [ROLE_A, ROLE_B]);
    assert!(plan.remove_role_ids.is_empty());
    assert_eq!(plan.outcome, SettledOutcome::Assigned);
    // Removal side is catalogue-ordered too.
    let plan =
        plan_select_delta(&panel, &held(&[ROLE_A, ROLE_B]), &[ROLE_B.to_owned()]).expect("plans");
    assert_eq!(plan.remove_role_ids, [ROLE_A]);
    // Deselect-all is the fallback: a null option with no role, removing held
    // panel roles (legacy audits `optionKey: null`).
    let plan = plan_select_delta(&panel, &held(&[ROLE_A]), &[]).expect("plans");
    assert_eq!(plan.option_key, None);
    assert_eq!(plan.role_id, None);
    assert_eq!(plan.remove_role_ids, [ROLE_A]);
    assert_eq!(plan.outcome, SettledOutcome::Removed);
    // Deselect-all with nothing held is an idempotent no-op.
    let plan = plan_select_delta(&panel, &held(&[]), &[]).expect("plans");
    assert!(plan.add_role_ids.is_empty() && plan.remove_role_ids.is_empty());
    assert_eq!(plan.outcome, SettledOutcome::AlreadyAbsent);
}

#[test]
fn session_picks_route_valid_in_catalog_order_despite_stale_keys() {
    let catalog = build_session_picks("777777777777777777", "888888888888888888");
    let visible = |_: &str| true;
    // Stale key plus reversed, duplicated submission: valid picks still route,
    // in catalog order, deduped; the stale key is reported, never routed.
    let plan = plan_session(
        &["bogus-key", "join-voice", "find-players", "join-voice"],
        &visible,
        &catalog,
    );
    assert_eq!(plan.picks, ["find-players", "join-voice"]);
    assert_eq!(
        plan.channel_ids,
        ["777777777777777777", "888888888888888888"]
    );
    assert!(plan.unavailable.is_empty());
    assert_eq!(plan.unknown_keys, ["bogus-key"]);
}

#[test]
fn session_picks_withhold_invisible_fallback_destinations() {
    let catalog = build_session_picks("777777777777777777", "888888888888888888");
    // The lobby is closed to this member: the pick stays known but has no
    // route, and only the visible room links.
    let visible = |id: &str| id == "777777777777777777";
    let plan = plan_session(&["find-players", "join-voice"], &visible, &catalog);
    assert_eq!(plan.picks, ["find-players", "join-voice"]);
    assert_eq!(plan.channel_ids, ["777777777777777777"]);
    assert_eq!(plan.unavailable, ["join-voice"]);
    // Nothing routable and nothing known: no links at all.
    let plan = plan_session(&["bogus-key"], &visible, &catalog);
    assert!(plan.channel_ids.is_empty());
    assert_eq!(plan.unknown_keys, ["bogus-key"]);
    // Destination-level fallback: an invisible primary falls back to the hub
    // (flagged degraded); a fully invisible pick has no route at all.
    let pick = GamePick {
        key: "synthetic",
        label: "Synthetic",
        description: "Synthetic fixture pick.",
        emoji: "🎲",
        role_id: ROLE_A,
        role_name: "Synthetic",
        primary_channel_id: Some("999999999999999999"),
        fallback_channel_id: "777777777777777777",
    };
    let routed = resolve_destination(&pick, &visible);
    assert_eq!(routed.channel_id.as_deref(), Some("777777777777777777"));
    assert!(routed.degraded);
    let dark = resolve_destination(&pick, &|_: &str| false);
    assert_eq!(dark.channel_id, None);
}

#[test]
fn game_selection_dedupes_and_reports_unknown_keys() {
    // First occurrence wins for known and unknown keys alike; unknowns are
    // reported, never routed.
    let visible = |_: &str| true;
    let selection = plan_game_selection(&["shooters", "nope", "shooters"], &visible);
    assert_eq!(selection.unknown_keys, ["nope"]);
    assert_eq!(selection.role_ids, ["1051272877871222915"]);
}
