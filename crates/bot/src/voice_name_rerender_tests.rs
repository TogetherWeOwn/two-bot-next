//! Acceptance tests: custom-name re-render on room changes (V5 recalculation).
//!
//! A room with a stored `/name` override must re-render that override through
//! the sanitizer plus name filter when the facts the override reads change
//! (occupancy, activity, limit), reusing the existing rename coalescer. The
//! V5 tick entry is `refresh_template_names`, the same pass that already
//! re-renders template-named rooms. Discord and the database are faked
//! behind `RoomWrites` and `RoomPersistence`; nothing here touches a network
//! or a live database.
//!
//! The builder must preserve this file verbatim (see the manifest on the
//! acceptance card). The occupancy, activity, limit, churn and filter cases
//! fail until the recalculation is wired; the template, ownership and
//! deletion controls pass today and must stay green.

use super::*;
use two_bot_core::voice_presence::MemberPresence;

/// One tracked room (500, owned by `MEMBER`, display name "Alex") under a
/// creator carrying `template`, optionally holding a stored custom override.
async fn rerender_worker(
    custom: Option<&str>,
    template: &str,
) -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = fixture();
    store.creators.lock().unwrap()[0].name_template = template.to_owned();
    store.rooms.lock().unwrap().insert(500, room(500));
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    live.voice_update(MEMBER, Some(500), Some(false));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.name_directory.insert(MEMBER, "Alex".to_owned());
    if let Some(custom) = custom {
        worker.custom_names.insert(500, custom.to_owned());
    }
    (worker, trace)
}

fn renames_of(trace: &Trace) -> Vec<String> {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("rename:"))
        .cloned()
        .collect()
}

fn override_saves_of(trace: &Trace) -> Vec<String> {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("save_custom_name"))
        .cloned()
        .collect()
}

// --- re-render on fact changes (failing until V5 recalculation is wired) ----

#[tokio::test]
async fn occupancy_change_rerenders_the_custom_override() {
    let (mut worker, trace) =
        rerender_worker(Some("@@owner@@ · @@num@@/@@limit@@"), "Template @@num@@").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(renames_of(&trace), ["rename:500:Alex · 1/8"]);
    // A re-render is a rename only: the stored override is not written again,
    // so `name_touched_at` keeps measuring set/restore, not recalculation.
    assert!(override_saves_of(&trace).is_empty(), "{trace:?}");
    // Nothing changed: no new proposal, nothing to dispatch.
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);

    // A second occupant joins: the override re-renders, not the creator
    // template, and the stored text keeps its template syntax.
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(2);
    assert_eq!(worker.desired_names[&500], "Alex · 2/8");
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        renames_of(&trace),
        ["rename:500:Alex · 1/8", "rename:500:Alex · 2/8"]
    );
    assert_eq!(
        worker.custom_names.get(&500).map(String::as_str),
        Some("@@owner@@ · @@num@@/@@limit@@")
    );
    assert!(override_saves_of(&trace).is_empty(), "{trace:?}");
}

#[tokio::test]
async fn activity_change_rerenders_the_custom_override() {
    let (mut worker, trace) = rerender_worker(
        Some("{{PLAYING ?? @@game_name@@ // Hangout}}{{ANY_LIVE ?? 🔴}}"),
        "Template @@num@@",
    )
    .await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(renames_of(&trace), ["rename:500:Hangout"]);

    // The owner starts a game, then goes live: both presence changes reach
    // the override's next render.
    let room = worker.live.set_presence(
        MEMBER,
        MemberPresence {
            game: Some("Apex Legends".to_owned()),
            ..MemberPresence::default()
        },
    );
    assert_eq!(room, Some(500));
    worker.room_facts_changed(500, 1);
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&500], "Apex Legends");
    assert_eq!(worker.live.set_self_stream(MEMBER, true), Some(500));
    worker.room_facts_changed(500, 2);
    worker.refresh_template_names(2);
    assert_eq!(worker.desired_names[&500], "Apex Legends 🔴");

    // The rename lane coalesces both changes into one paced rename.
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        renames_of(&trace),
        ["rename:500:Hangout", "rename:500:Apex Legends 🔴"]
    );
}

#[tokio::test]
async fn limit_change_rerenders_the_custom_override() {
    let (mut worker, trace) =
        rerender_worker(Some("@@num@@/@@limit@@ den"), "Template @@num@@").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(renames_of(&trace), ["rename:500:1/8 den"]);

    // The channel's user limit drops from the fixture's 8 to 4.
    worker
        .live
        .write_state()
        .channels
        .get_mut(&500)
        .unwrap()
        .user_limit = Some(4);
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&500], "1/4 den");
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        renames_of(&trace),
        ["rename:500:1/8 den", "rename:500:1/4 den"]
    );
}

#[tokio::test]
async fn churn_burst_coalesces_to_one_paced_rename() {
    let (mut worker, trace) = rerender_worker(Some("@@num@@ here"), "Template @@num@@").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(renames_of(&trace), ["rename:500:1 here"]);

    // Rapid joins and a leave, each followed by a naming pass well inside
    // Discord's rename budget: at most one pending name may survive, and no
    // rename may leave before the lane paces it.
    for (step, member, present) in [
        (1u64, MEMBER + 1, true),
        (2, MEMBER + 2, true),
        (3, MEMBER + 3, true),
        (4, MEMBER + 3, false),
        (5, MEMBER + 4, true),
    ] {
        worker
            .live
            .voice_update(member, present.then_some(500), Some(false));
        worker.refresh_template_names(step);
    }
    assert_eq!(worker.desired_names[&500], "4 here");
    assert!(!worker.dispatch_one(6).await, "the lane paces the burst");

    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        renames_of(&trace),
        ["rename:500:1 here", "rename:500:4 here"],
        "the whole burst lands as exactly one further rename"
    );
}

#[tokio::test]
async fn a_rerender_the_filter_blocks_keeps_the_old_name_and_the_override() {
    let (worker, trace) = rerender_worker(
        Some("{{PLAYING ?? @@game_name@@ // Hangout}}"),
        "Template @@num@@",
    )
    .await;
    let mut worker = worker.with_name_policy(Arc::new(fixture_policy(&["blorp"])));
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    assert_eq!(renames_of(&trace), ["rename:500:Hangout"]);

    // A member-controlled activity title the name filter blocks must never
    // become the channel name, and the blocked render must not clear the
    // stored override.
    worker.live.set_presence(
        MEMBER,
        MemberPresence {
            game: Some("blorp quest".to_owned()),
            ..MemberPresence::default()
        },
    );
    worker.room_facts_changed(500, 1);
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&500], "Hangout");
    let _ = worker.dispatch_one(600_000).await;
    assert_eq!(renames_of(&trace), ["rename:500:Hangout"]);
    assert_eq!(
        worker.custom_names.get(&500).map(String::as_str),
        Some("{{PLAYING ?? @@game_name@@ // Hangout}}")
    );

    // A later clean activity renders again, sanitized: no character the
    // sanitizer strips may survive into the proposed name.
    worker.live.set_presence(
        MEMBER,
        MemberPresence {
            game: Some("Apex` Legends".to_owned()),
            ..MemberPresence::default()
        },
    );
    worker.room_facts_changed(500, 2);
    worker.refresh_template_names(2);
    let desired = worker.desired_names[&500].clone();
    assert!(desired.starts_with("Apex"), "{desired}");
    assert!(!desired.contains('`'), "{desired}");
    assert!(!desired.contains('@'), "{desired}");
    dispatch(&mut worker, 600_001).await;
    assert_eq!(
        renames_of(&trace),
        vec![
            "rename:500:Hangout".to_owned(),
            format!("rename:500:{desired}")
        ]
    );
}

// --- controls (green today; the recalculation must not break them) ----------

#[tokio::test]
async fn control_template_named_room_still_rerenders_from_its_template() {
    let (mut worker, trace) = rerender_worker(None, "@@owner@@ · @@num@@").await;
    worker.refresh_template_names(0);
    dispatch(&mut worker, 0).await;
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&500], "Alex · 2");
    dispatch(&mut worker, 600_000).await;
    assert_eq!(
        renames_of(&trace),
        ["rename:500:Alex · 1", "rename:500:Alex · 2"]
    );
    assert!(worker.custom_names.is_empty());
}

#[tokio::test]
async fn control_ownership_change_keeps_the_custom_override() {
    let (mut worker, _trace) = rerender_worker(Some("Kept @@num@@"), "Template @@num@@").await;
    worker.live.voice_update(MEMBER + 1, Some(500), Some(false));
    // The owner leaves; succession hands the room to the remaining occupant
    // and the override stays with the room.
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.reconcile();
    assert_eq!(worker.rooms[&500].owner_id, MEMBER + 1);
    assert_eq!(
        worker.custom_names.get(&500).map(String::as_str),
        Some("Kept @@num@@")
    );
}

#[tokio::test]
async fn control_room_deletion_drops_the_custom_override() {
    let (mut worker, _trace) = rerender_worker(Some("Kept @@num@@"), "Template @@num@@").await;
    // This scenario asserts immediate deletion, so it shortens the
    // empty-room grace; the grace itself is pinned elsewhere.
    worker.live.set_empty_grace(Duration::ZERO);
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.reconcile();
    for step in 0..8u64 {
        worker.dispatch_one(step * 1_000_000).await;
    }
    assert!(worker.tracked().is_empty());
    assert!(worker.custom_names.is_empty());
}
