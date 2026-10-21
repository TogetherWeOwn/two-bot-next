#![cfg(test)]
//! Supplement the protected runtime matrix with transport-guard and projection cases.

use super::*;

fn grant() -> PermissionOverwrite {
    PermissionOverwrite {
        allow: Permissions::CONNECT,
        deny: Permissions::empty(),
        id: Id::new(401),
        kind: PermissionOverwriteType::Member,
    }
}

fn invalidate(live: &LiveGuild, event: &str) {
    match event {
        "acl" => set_room_overwrites(
            live,
            vec![role_overwrite(
                GUILD,
                Permissions::SPEAK,
                Permissions::empty(),
            )],
        ),
        "generation" => {
            // Identical ACLs in a new authoritative generation are still stale.
            live.publish(snapshot(&[ROOM], vec![]));
        }
        "unready" => live.disconnect(),
        "deleted" => live.remove_channel(ROOM),
        "permission" => live.refresh_bot(BotAccess {
            member_id: BOT,
            guild_owner_id: 998,
            system_channel_id: None,
            member_roles: vec![],
            roles: vec![role(permissions() - Permissions::MANAGE_ROLES)],
        }),
        "unknown" => {
            let mut room = channel(ROOM, 2, Some(CATEGORY));
            room.permission_overwrites = None;
            live.upsert_channel(room);
        }
        _ => panic!("unknown fixture event"),
    }
}

#[tokio::test]
async fn overwrite_snapshot_guard_refuses_each_stale_case_before_transport() {
    for event in [
        "acl",
        "generation",
        "unready",
        "deleted",
        "permission",
        "unknown",
    ] {
        let (live, _, http, trace) = parts();
        let snapshot = live.overwrite_snapshot(ROOM).unwrap();
        let guard = live.overwrite_guard(ROOM, snapshot.generation, snapshot.overwrites);
        assert!(guard(), "{event}: initially valid");
        invalidate(&live, event);
        assert_eq!(
            http.put_overwrite(ROOM, grant(), guard).await,
            Err(RoomHttpError::Cancelled),
            "{event}"
        );
        assert!(
            http.written_overwrites.lock().unwrap().is_empty(),
            "{event}"
        );
        assert!(calls(&trace).is_empty(), "{event}: never reached transport");
    }
}

#[test]
fn overwrite_snapshot_projection_preserves_each_newer_observation() {
    for event in [
        "acl",
        "generation",
        "unready",
        "deleted",
        "permission",
        "unknown",
    ] {
        let (live, _, _, _) = parts();
        let snapshot = live.overwrite_snapshot(ROOM).unwrap();
        let next = private_runtime::with_overwrite(&snapshot.overwrites, &grant());
        invalidate(&live, event);
        let observed = live.read_state().channels.get(&ROOM).cloned();
        assert!(
            !live.advance_overwrites(ROOM, snapshot.generation, &snapshot.overwrites, &next),
            "{event}"
        );
        assert_eq!(
            live.read_state().channels.get(&ROOM).cloned(),
            observed,
            "{event}"
        );
    }
}

#[test]
fn overwrite_snapshot_projection_accepts_prior_and_reordered_own_echo() {
    let (live, _, _, _) = parts();
    let snapshot = live.overwrite_snapshot(ROOM).unwrap();
    let next = private_runtime::with_overwrite(&snapshot.overwrites, &grant());
    assert!(live.advance_overwrites(ROOM, snapshot.generation, &snapshot.overwrites, &next));
    let mut echoed = next.clone();
    echoed.reverse();
    set_room_overwrites(&live, echoed.clone());
    let revision = live.read_state().channel_revisions[&ROOM];
    assert!(live.advance_overwrites(ROOM, snapshot.generation, &snapshot.overwrites, &next));
    let state = live.read_state();
    assert_eq!(state.channels[&ROOM].permission_overwrites, Some(echoed));
    assert_eq!(
        state.channel_revisions[&ROOM], revision,
        "echo needs no projection"
    );
}

#[test]
fn overwrite_snapshot_unknown_missing_and_unready_are_not_empty_acls() {
    for event in ["unknown", "deleted", "unready"] {
        let (live, _, _, _) = parts();
        invalidate(&live, event);
        assert!(live.overwrite_snapshot(ROOM).is_none(), "{event}");
    }
}
