//! Time-aware name facts: room and game tiers, time of day.

use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_naming::{daypart, minutes_tier, RoomContext, MINUTE_TIERS};
use two_bot_core::voice_template::resolve_room_name;

/// 2026-10-10 00:00:00 UTC.
const MIDNIGHT: i64 = 1_791_590_400;

fn render(template: &str, ctx: &RoomContext) -> String {
    resolve_room_name(template, ctx, &ConditionFacts::default(), "fallback")
}

fn at_hour(hour: i64) -> RoomContext {
    RoomContext {
        timestamp: MIDNIGHT + hour * 3600,
        ..RoomContext::default()
    }
}

#[test]
fn minute_tiers_start_at_their_bounds() {
    assert_eq!(MINUTE_TIERS, [0, 15, 45, 90, 180, 360]);
    for (tier, bound) in MINUTE_TIERS.iter().enumerate() {
        assert_eq!(minutes_tier(*bound), tier as u32, "{bound}");
        if *bound > 0 {
            assert_eq!(minutes_tier(bound - 1), tier as u32 - 1, "{bound}");
        }
    }
    assert_eq!(minutes_tier(u32::MAX), 5);
}

#[test]
fn every_hour_has_one_daypart() {
    let expected = [
        "night", "night", "late night", "late night", "late night", "morning", "morning",
        "morning", "morning", "morning", "morning", "morning", "afternoon", "afternoon",
        "afternoon", "afternoon", "afternoon", "evening", "evening", "evening", "evening",
        "evening", "night", "night",
    ];
    for (hour, part) in expected.iter().enumerate() {
        assert_eq!(daypart(hour as u32), *part, "{hour}");
        assert_eq!(render("@@daypart@@", &at_hour(hour as i64)), *part, "{hour}");
    }
}

#[test]
fn time_tokens_render_minutes_and_tiers() {
    let ctx = RoomContext {
        room_minutes: 50,
        game_minutes: 200,
        ..RoomContext::default()
    };
    assert_eq!(
        render("@@room_minutes@@ @@room_tier@@ @@game_minutes@@ @@game_tier@@", &ctx),
        "50 2 200 4"
    );
}

#[test]
fn tiers_and_dayparts_drive_conditions() {
    let template = "{{@@room_tier@@ >= 3 ?? marathon // {{@@game_minutes@@ > 60 ?? grinding // fresh}}}}";
    let mut ctx = RoomContext::default();
    assert_eq!(render(template, &ctx), "fresh");
    ctx.game_minutes = 61;
    assert_eq!(render(template, &ctx), "grinding");
    ctx.room_minutes = 90;
    assert_eq!(render(template, &ctx), "marathon");

    let parts = "{{MORNING ?? m // {{AFTERNOON ?? a // {{EVENING ?? e // {{NIGHT ?? n // {{LATE_NIGHT ?? l // none}}}}}}}}}}";
    for (hour, letter) in [(6, "m"), (13, "a"), (19, "e"), (23, "n"), (0, "n"), (3, "l")] {
        assert_eq!(render(parts, &at_hour(hour)), letter, "{hour}");
    }
    // The guild offset moves the daypart with the local clock.
    let shifted = RoomContext {
        tz_offset_minutes: -5 * 60,
        ..at_hour(13)
    };
    assert_eq!(render(parts, &shifted), "m");
}
