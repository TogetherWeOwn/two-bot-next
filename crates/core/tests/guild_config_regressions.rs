//! TOG-9970 guild-config regressions. In-process loopback only; no credentials.
use axum::{body::Body, extract::State, http::Method, response::Response, routing::get, Router};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use two_bot_core::backup::{
    guild_config_api::GuildConfigDiscordApi,
    guild_config_restore::{
        apply_restore_plan, plan_restore, remap_snapshot_ids, resolve_path, resolve_value,
        snapshots_equal, RestoreOperation, RestorePath, RestorePlan,
    },
};

fn snapshot() -> Map<String, Value> {
    serde_json::from_value(json!({
        "version": 1, "guildId": "g", "guild": {
            "name": "Guild", "system_channel_id": null, "rules_channel_id": null,
            "public_updates_channel_id": null, "afk_channel_id": null
        },
        "roles": [
            {"id": "g", "name": "@everyone", "managed": false, "permissions": "0", "position": 0},
            {"id": "r1", "name": "Member", "managed": false, "permissions": "0", "position": 1}
        ],
        "channels": [
            {"id": "c1", "name": "alpha", "type": 0, "parent_id": null, "position": 0,
             "topic": "alpha topic", "permission_overwrites": []},
            {"id": "c2", "name": "beta", "type": 0, "parent_id": null, "position": 1,
             "topic": "beta topic", "permission_overwrites": []}
        ], "emojis": []
    }))
    .unwrap()
}

fn role(id: &str, name: &str, position: i64, managed: bool) -> Value {
    json!({"id": id, "name": name, "position": position, "managed": managed, "permissions": "0"})
}

fn category(id: &str, name: &str, position: i64) -> Value {
    json!({"id": id, "name": name, "type": 4, "parent_id": null, "position": position, "permission_overwrites": []})
}

fn with_authority(mut source: Map<String, Value>) -> Map<String, Value> {
    let mut bot = role("botrole", "Bot", 10, true);
    bot["permissions"] = json!("8");
    source["roles"]
        .as_array_mut()
        .unwrap()
        .extend([bot, role("ownerrole", "Owner", 20, false)]);
    source
}

#[derive(Debug)]
struct FakeState {
    snapshot: Map<String, Value>,
    writes: Vec<(String, String, Value)>,
    next_id: usize,
}

struct FakeDiscord {
    api: GuildConfigDiscordApi,
    state: Arc<Mutex<FakeState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(body: Value) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn member() -> Response {
    response(json!({"roles": ["botrole"]}))
}

fn merge(target: &mut Value, patch: &Value) {
    for (key, value) in patch.as_object().unwrap() {
        target[key] = value.clone();
    }
}

async fn write(
    State(state): State<Arc<Mutex<FakeState>>>,
    method: Method,
    uri: axum::http::Uri,
    body: Body,
) -> Response {
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !body.to_string().contains("restoreReference"),
        "all dependencies must resolve before transport"
    );
    let path = uri.path();
    let mut state = state.lock().unwrap();
    state
        .writes
        .push((method.to_string(), path.to_owned(), body.clone()));
    if method == Method::POST {
        state.next_id += 1;
        let id = (9_000_000_000_000_000_000u64 + state.next_id as u64).to_string();
        let field = match path {
            "/guilds/g/roles" => "roles",
            "/guilds/g/channels" => "channels",
            "/guilds/g/emojis" => "emojis",
            _ => panic!("unexpected POST {path}"),
        };
        let mut created = body;
        created["id"] = json!(id);
        if field == "roles" {
            created["managed"] = json!(false);
            created["position"] = json!(0);
        } else if field == "channels" {
            created["permission_overwrites"] = json!([]);
            if created.get("parent_id").is_none() {
                created["parent_id"] = Value::Null;
            }
        }
        state.snapshot[field].as_array_mut().unwrap().push(created);
        return response(json!({"id": id}));
    }
    if path == "/guilds/g" {
        let guild = state.snapshot.get_mut("guild").unwrap();
        merge(guild, &body);
    } else if path == "/guilds/g/roles" || path == "/guilds/g/channels" {
        let field = if path.ends_with("roles") {
            "roles"
        } else {
            "channels"
        };
        for position in body.as_array().unwrap() {
            let target = state.snapshot[field]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["id"] == position["id"])
                .unwrap();
            merge(target, position);
        }
    } else {
        let (field, id) = if let Some(id) = path.strip_prefix("/guilds/g/roles/") {
            ("roles", id)
        } else if let Some(id) = path.strip_prefix("/channels/") {
            ("channels", id)
        } else if let Some(id) = path.strip_prefix("/guilds/g/emojis/") {
            ("emojis", id)
        } else {
            panic!("unexpected PATCH {path}");
        };
        let target = state.snapshot[field]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["id"].as_str() == Some(id))
            .unwrap();
        merge(target, &body);
    }
    // Minimal synthetic resource receipt; target/delta assertions inspect writes.
    response(json!({"id": "1"}))
}

async fn fake(live: Map<String, Value>) -> FakeDiscord {
    let state = Arc::new(Mutex::new(FakeState {
        snapshot: live,
        writes: vec![],
        next_id: 0,
    }));
    let app = Router::new()
        .route("/{*path}", get(member).post(write).patch(write))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let api = GuildConfigDiscordApi::new(
        Some(&base),
        Some(&base),
        "not-a-token".into(),
        "bot".into(),
        "g".into(),
    )
    .unwrap();
    FakeDiscord { api, state, task }
}

async fn converge(source: &Map<String, Value>, live: Map<String, Value>) -> RestorePlan {
    let plan = plan_restore(source, &live).unwrap();
    let mut fake = fake(live).await;
    let ids = apply_restore_plan(&mut fake.api, &plan).await.unwrap();
    let after = fake.state.lock().unwrap().snapshot.clone();
    assert!(
        snapshots_equal(&remap_snapshot_ids(source, &ids), &after),
        "restore must converge"
    );
    assert_eq!(fake.api.writes as usize, plan.operations.len());
    assert_eq!(
        plan_restore(&remap_snapshot_ids(source, &ids), &after)
            .unwrap()
            .counts
            .operations,
        0
    );
    plan
}

fn paths(plan: &RestorePlan) -> Vec<String> {
    plan.operations
        .iter()
        .map(|op| resolve_path(&op.path, &plan.known_ids.channels).unwrap())
        .collect()
}

#[tokio::test]
async fn duplicate_surviving_role_names_preserve_membership_identity() {
    let mut source = snapshot();
    let mut second = role("r2", "Member", 2, false);
    second["permissions"] = json!("8");
    source["roles"].as_array_mut().unwrap().push(second);
    let identical = plan_restore(&source, &source).unwrap();
    assert_eq!(identical.known_ids.roles["r1"], "r1");
    assert_eq!(identical.known_ids.roles["r2"], "r2");
    assert_eq!(identical.counts.operations, 0);
    let mut live = source.clone();
    live["roles"][1]["permissions"] = json!("1");
    let plan = converge(&source, live).await;
    assert_eq!(paths(&plan), ["/guilds/g/roles/r1"]);
}

#[tokio::test]
async fn renamed_and_swapped_roles_patch_their_surviving_ids() {
    let mut source = snapshot();
    source["roles"]
        .as_array_mut()
        .unwrap()
        .push(role("r2", "Other", 2, false));
    let mut live = source.clone();
    live["roles"][1]["name"] = json!("Other");
    live["roles"][2]["name"] = json!("Member");
    let plan = converge(&source, live).await;
    assert_eq!(plan.known_ids.roles["r1"], "r1");
    assert_eq!(plan.known_ids.roles["r2"], "r2");
    assert_eq!(paths(&plan), ["/guilds/g/roles/r1", "/guilds/g/roles/r2"]);
}

#[test]
fn role_fallback_requires_unique_unclaimed_compatible_target() {
    let source = snapshot();
    let mut live = source.clone();
    live["roles"][1]["id"] = json!("replacement");
    assert_eq!(
        plan_restore(&source, &live).unwrap().known_ids.roles["r1"],
        "replacement"
    );
    live["roles"]
        .as_array_mut()
        .unwrap()
        .push(role("another", "Member", 1, false));
    assert!(plan_restore(&source, &live)
        .unwrap_err()
        .0
        .contains("ambiguous"));
    live["roles"].as_array_mut().unwrap().pop();
    live["roles"][1]["managed"] = json!(true);
    let plan = plan_restore(&source, &live).unwrap();
    assert!(!plan.known_ids.roles.contains_key("r1"));
    assert!(plan
        .operations
        .iter()
        .any(|op| op.label == "create role Member"));
}

#[test]
fn role_fallback_cannot_claim_another_surviving_source_or_reuse_a_fallback() {
    let mut source = snapshot();
    source["roles"]
        .as_array_mut()
        .unwrap()
        .push(role("r2", "Other", 2, false));
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().remove(1);
    live["roles"][1]["name"] = json!("Member");
    assert!(plan_restore(&source, &live)
        .unwrap_err()
        .0
        .contains("already claimed"));
    source["roles"][2]["name"] = json!("Member");
    live["roles"][1]["id"] = json!("replacement");
    assert!(plan_restore(&source, &live)
        .unwrap_err()
        .0
        .contains("injective"));
}

#[tokio::test]
async fn swapped_channel_names_preserve_topics_and_history_identity() {
    let source = snapshot();
    let mut live = source.clone();
    live["channels"][0]["name"] = json!("beta");
    live["channels"][1]["name"] = json!("alpha");
    let plan = converge(&source, live).await;
    assert_eq!(plan.known_ids.channels["c1"], "c1");
    assert_eq!(plan.known_ids.channels["c2"], "c2");
    assert_eq!(paths(&plan), ["/channels/c1", "/channels/c2"]);
}

#[test]
fn duplicate_surviving_channels_and_categories_are_not_ambiguous() {
    let mut source = snapshot();
    source["channels"][1]["name"] = json!("alpha");
    source["channels"].as_array_mut().unwrap().extend([
        category("cat1", "Category", 2),
        category("cat2", "Category", 3),
    ]);
    let plan = plan_restore(&source, &source).unwrap();
    assert_eq!(plan.counts.operations, 0);
    for id in ["c1", "c2", "cat1", "cat2"] {
        assert_eq!(plan.known_ids.channels[id], id);
    }
}

#[test]
fn channel_and_category_fallbacks_refuse_ambiguity_and_noninjectivity() {
    for category_case in [false, true] {
        let mut source = snapshot();
        if category_case {
            source["channels"] = json!([category("c1", "alpha", 0), category("c2", "beta", 1)]);
        }
        let mut live = source.clone();
        live["channels"][0]["id"] = json!("replacement");
        assert_eq!(
            plan_restore(&source, &live).unwrap().known_ids.channels["c1"],
            "replacement"
        );
        let mut duplicate = live["channels"][0].clone();
        duplicate["id"] = json!("another");
        live["channels"].as_array_mut().unwrap().push(duplicate);
        assert!(plan_restore(&source, &live)
            .unwrap_err()
            .0
            .contains("ambiguous"));
        live["channels"].as_array_mut().unwrap().pop();
        source["channels"][1]["name"] = json!("alpha");
        live["channels"].as_array_mut().unwrap().remove(1);
        assert!(plan_restore(&source, &live)
            .unwrap_err()
            .0
            .contains("injective"));
        live["channels"][0]["id"] = json!("c2");
        assert!(plan_restore(&source, &live)
            .unwrap_err()
            .0
            .contains("already claimed"));
    }
}

#[test]
fn channel_id_match_requires_compatible_type() {
    let source = snapshot();
    let mut live = source.clone();
    live["channels"][0]["type"] = json!(2);
    let mut replacement = source["channels"][0].clone();
    replacement["id"] = json!("text-replacement");
    live["channels"].as_array_mut().unwrap().push(replacement);
    let plan = plan_restore(&source, &live).unwrap();
    assert_eq!(plan.known_ids.channels["c1"], "text-replacement");
}

#[tokio::test]
async fn renamed_categories_and_moved_channels_patch_surviving_ids() {
    let mut source = snapshot();
    source["channels"]
        .as_array_mut()
        .unwrap()
        .extend([category("cat1", "First", 2), category("cat2", "Second", 3)]);
    source["channels"][0]["parent_id"] = json!("cat1");
    source["channels"][1]["parent_id"] = json!("cat2");
    source["channels"][1]["name"] = json!("alpha");
    let mut live = source.clone();
    live["channels"][2]["name"] = json!("Second");
    live["channels"][3]["name"] = json!("First");
    live["channels"][0]["parent_id"] = json!("cat2");
    let plan = converge(&source, live).await;
    for id in ["c1", "c2", "cat1", "cat2"] {
        assert_eq!(plan.known_ids.channels[id], id);
    }
    assert!(plan.operations.iter().all(|op| op.method == "PATCH"));
    let channel_ops: Vec<_> = plan
        .operations
        .iter()
        .filter(|op| matches!(&op.path, RestorePath::Channel(id) if id == "c1"))
        .collect();
    assert_eq!(
        channel_ops.len(),
        1,
        "a parent move needs only one per-channel PATCH"
    );
    let body = resolve_value(
        &channel_ops[0].body,
        &plan.known_ids.roles,
        &plan.known_ids.channels,
    )
    .unwrap();
    assert_eq!(body["parent_id"], "cat1");
}

#[tokio::test]
async fn surviving_channel_moves_to_created_parent_or_guild_root() {
    for to_root in [false, true] {
        let mut source = snapshot();
        source["channels"]
            .as_array_mut()
            .unwrap()
            .push(category("cat1", "Category", 2));
        if !to_root {
            source["channels"][0]["parent_id"] = json!("cat1");
        }
        let mut live = source.clone();
        if to_root {
            live["channels"][0]["parent_id"] = json!("cat1");
        } else {
            live["channels"].as_array_mut().unwrap().pop();
        }
        let plan = converge(&source, live).await;
        assert_eq!(plan.known_ids.channels["c1"], "c1");
        assert!(plan
            .operations
            .iter()
            .all(|op| op.label != "create channel alpha"));
    }
}

#[tokio::test]
async fn everyone_permission_drift_converges_without_creation_or_position() {
    let source = snapshot();
    let mut live = source.clone();
    live["roles"][0]["permissions"] = json!("8");
    let plan = converge(&source, live).await;
    assert_eq!(plan.operations.len(), 1);
    let op = &plan.operations[0];
    assert_eq!(op.method, "PATCH");
    assert_eq!(paths(&plan), ["/guilds/g/roles/g"]);
    assert_eq!(op.body, json!({"permissions": "0"}));
    assert!(op.capture_id.is_none());
}

#[test]
fn literal_role_member_and_everyone_overwrite_sets_are_idempotent_and_order_independent() {
    let mut source = snapshot();
    source["channels"]
        .as_array_mut()
        .unwrap()
        .push(category("cat1", "Category", 2));
    let overwrites = json!([
        {"id": "r1", "type": 0, "allow": "1024", "deny": "0"},
        {"id": "member1", "type": 1, "allow": "0", "deny": "2048"},
        {"id": "g", "type": 0, "allow": "0", "deny": "1024"}
    ]);
    for channel in source["channels"].as_array_mut().unwrap() {
        channel["permission_overwrites"] = overwrites.clone();
    }
    assert_eq!(plan_restore(&source, &source).unwrap().counts.operations, 0);
    let mut live = source.clone();
    for channel in live["channels"].as_array_mut().unwrap() {
        channel["permission_overwrites"]
            .as_array_mut()
            .unwrap()
            .reverse();
    }
    assert_eq!(plan_restore(&source, &live).unwrap().counts.operations, 0);
    live["roles"][1]["id"] = json!("replacement");
    for channel in live["channels"].as_array_mut().unwrap() {
        for overwrite in channel["permission_overwrites"].as_array_mut().unwrap() {
            if overwrite["id"] == "r1" {
                overwrite["id"] = json!("replacement");
            }
        }
    }
    let mapped = plan_restore(&source, &live).unwrap();
    assert_eq!(mapped.known_ids.roles["r1"], "replacement");
    assert_eq!(mapped.counts.operations, 0);
}

#[tokio::test]
async fn settings_only_drift_with_unchanged_overwrites_needs_no_manage_roles() {
    let mut source = with_authority(snapshot());
    source["roles"][2]["permissions"] = json!((1_u64 << 5).to_string());
    source["channels"][0]["permission_overwrites"] =
        json!([{"id": "ownerrole", "type": 0, "allow": "0", "deny": "0"}]);
    let mut live = source.clone();
    live["guild"]["name"] = json!("Renamed Guild");
    let plan = plan_restore(&source, &live).unwrap();
    assert_eq!(plan.counts.roles, 0);
    assert_eq!(plan.counts.overwrites, 0);
    let fake = fake(live.clone()).await;
    fake.api
        .assert_restore_permissions(&live, &plan)
        .await
        .unwrap();
}

#[tokio::test]
async fn unrelated_higher_role_does_not_block_lower_role_patch_or_position() {
    for position_change in [false, true] {
        let source = with_authority(snapshot());
        let mut live = source.clone();
        if position_change {
            live["roles"][1]["position"] = json!(2);
        } else {
            live["roles"][1]["permissions"] = json!("1");
        }
        let plan = plan_restore(&source, &live).unwrap();
        assert_eq!(plan.counts.roles, 1);
        let fake = fake(live.clone()).await;
        fake.api
            .assert_restore_permissions(&live, &plan)
            .await
            .unwrap();
        if position_change {
            assert_eq!(
                plan.operations[0].body.as_array().unwrap().len(),
                1,
                "unchanged high roles must not become position targets"
            );
        }
    }
}

#[tokio::test]
async fn existing_and_new_role_positions_at_or_above_bot_are_refused() {
    for (new_role, position) in [(false, 10), (false, 11), (true, 10), (true, 11)] {
        let mut source = with_authority(snapshot());
        let mut live = source.clone();
        if new_role {
            source["roles"][1]["position"] = json!(position);
            live["roles"].as_array_mut().unwrap().remove(1);
        } else {
            live["roles"][1]["position"] = json!(position);
        }
        let plan = plan_restore(&source, &live).unwrap();
        let fake = fake(live.clone()).await;
        let err = fake
            .api
            .assert_restore_permissions(&live, &plan)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Member"), "{err}");
        assert_eq!(fake.api.writes, 0);
    }
    let mut source = with_authority(snapshot());
    source["roles"][1]["position"] = json!(10);
    let mut live = source.clone();
    live["roles"][1]["position"] = json!(1);
    let plan = plan_restore(&source, &live).unwrap();
    let fake = fake(live.clone()).await;
    assert!(fake
        .api
        .assert_restore_permissions(&live, &plan)
        .await
        .unwrap_err()
        .to_string()
        .contains("planned position 10"));
}

#[tokio::test]
async fn new_low_role_position_is_allowed_despite_unrelated_high_role() {
    let source = with_authority(snapshot());
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().remove(1);
    let plan = plan_restore(&source, &live).unwrap();
    let fake = fake(live.clone()).await;
    fake.api
        .assert_restore_permissions(&live, &plan)
        .await
        .unwrap();
}

#[tokio::test]
async fn overwrite_hierarchy_checks_resolved_live_ids_and_removed_role_entries() {
    for removed in [false, true] {
        let mut source = with_authority(snapshot());
        source["roles"][1]["position"] = json!(20);
        let mut live = source.clone();
        live["roles"][1]["id"] = json!("replacement");
        if removed {
            live["channels"][0]["permission_overwrites"] =
                json!([{"id": "replacement", "type": 0, "allow": "0", "deny": "0"}]);
        } else {
            source["channels"][0]["permission_overwrites"] =
                json!([{"id": "r1", "type": 0, "allow": "0", "deny": "0"}]);
        }
        let plan = plan_restore(&source, &live).unwrap();
        assert_eq!(plan.counts.roles, 0);
        let fake = fake(live.clone()).await;
        assert!(fake
            .api
            .assert_restore_permissions(&live, &plan)
            .await
            .unwrap_err()
            .to_string()
            .contains("Member (20)"));
    }
}

async fn assert_refused_without_writes(
    source: Map<String, Value>,
    live: Map<String, Value>,
    dependency: &str,
) {
    let mut fake = fake(live.clone()).await;
    let result = match plan_restore(&source, &live) {
        Ok(plan) => apply_restore_plan(&mut fake.api, &plan).await.map(|_| ()),
        Err(err) => Err(err),
    };
    assert!(
        result.unwrap_err().0.contains(dependency),
        "must identify the bad dependency"
    );
    assert_eq!(fake.api.writes, 0);
    assert!(fake.state.lock().unwrap().writes.is_empty());
    assert!(
        plan_restore(&source, &live).is_err(),
        "refusal belongs at plan time, not just apply"
    );
}

#[tokio::test]
async fn missing_managed_and_unknown_overwrite_roles_refuse_before_member_patch() {
    for managed in [false, true] {
        let mut source = snapshot();
        if managed {
            source["roles"].as_array_mut().unwrap().push(role(
                "integration",
                "Integration",
                2,
                true,
            ));
        }
        source["channels"][0]["permission_overwrites"] =
            json!([{"id": "integration", "type": 0, "allow": "1024", "deny": "0"}]);
        let mut live = source.clone();
        if managed {
            live["roles"].as_array_mut().unwrap().pop();
        }
        live["roles"][1]["permissions"] = json!("1");
        live["channels"][0]["permission_overwrites"] = json!([]);
        assert_refused_without_writes(source, live, "integration").await;
    }
}

#[tokio::test]
async fn unknown_guild_settings_and_parent_references_refuse_before_any_write() {
    for field in [
        "system_channel_id",
        "rules_channel_id",
        "public_updates_channel_id",
        "afk_channel_id",
        "parent_id",
    ] {
        let mut source = snapshot();
        let mut live = source.clone();
        live["roles"][1]["permissions"] = json!("1");
        if field == "parent_id" {
            source["channels"][0][field] = json!("ghost-channel");
        } else {
            source["guild"][field] = json!("ghost-channel");
        }
        assert_refused_without_writes(source, live, "ghost-channel").await;
    }
}

#[tokio::test]
async fn emoji_unknown_or_missing_managed_role_references_refuse_before_any_write() {
    for managed_role in [false, true] {
        for managed_emoji in [false, true] {
            let mut source = snapshot();
            if managed_role {
                source["roles"].as_array_mut().unwrap().push(role(
                    "integration",
                    "Integration",
                    2,
                    true,
                ));
            }
            source["emojis"] = json!([{"id": "e1", "name": "wave", "managed": managed_emoji, "roles": ["integration"]}]);
            let mut live = source.clone();
            if managed_role {
                live["roles"].as_array_mut().unwrap().pop();
            }
            live["roles"][1]["permissions"] = json!("1");
            // Keep the emoji reference unchanged: closure cannot depend solely
            // on whether an emoji PATCH happens to be emitted.
            assert_refused_without_writes(source, live, "integration").await;
        }
    }
}

#[tokio::test]
async fn unchanged_unknown_overwrite_reference_is_not_mistaken_for_a_known_live_id() {
    let mut source = snapshot();
    source["channels"][0]["permission_overwrites"] =
        json!([{"id": "ghost", "type": 0, "allow": "0", "deny": "0"}]);
    let mut live = source.clone();
    live["roles"][1]["permissions"] = json!("1");
    assert_refused_without_writes(source, live, "ghost").await;
}

#[tokio::test]
async fn supplied_plan_with_invalid_later_dependency_refuses_before_first_write() {
    let source = snapshot();
    let mut live = source.clone();
    live["roles"][1]["permissions"] = json!("1");
    for bad_path in [false, true] {
        let mut plan = plan_restore(&source, &live).unwrap();
        assert_eq!(plan.operations[0].label, "patch role Member");
        plan.operations.push(RestoreOperation {
            label: "bad later operation".into(), method: "PATCH".into(),
            path: if bad_path { RestorePath::Channel("ghost".into()) } else { RestorePath::Literal("/guilds/g".into()) },
            body: json!({"system_channel_id": {"restoreReference": "channel", "sourceId": "ghost"}}),
            capture_id: None,
        });
        let mut fake = fake(live.clone()).await;
        let err = apply_restore_plan(&mut fake.api, &plan).await.unwrap_err();
        assert!(err.0.contains("ghost"));
        assert_eq!(fake.api.writes, 0);
        assert!(fake.state.lock().unwrap().writes.is_empty());
    }
}

#[tokio::test]
async fn ordered_new_role_category_channel_settings_and_emoji_dependencies_resolve() {
    let mut source = snapshot();
    source["channels"]
        .as_array_mut()
        .unwrap()
        .push(category("cat1", "Category", 2));
    source["channels"][0]["parent_id"] = json!("cat1");
    source["channels"][0]["permission_overwrites"] =
        json!([{"id": "r1", "type": 0, "allow": "0", "deny": "1024"}]);
    source["guild"]["system_channel_id"] = json!("c1");
    source["emojis"] = json!([{"id": "e1", "name": "wave", "managed": false, "roles": ["r1"], "image": "data:image/png;base64,AA=="}]);
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().remove(1);
    live["channels"].as_array_mut().unwrap().pop();
    live["channels"].as_array_mut().unwrap().remove(0);
    live["guild"]["system_channel_id"] = Value::Null;
    live["emojis"] = json!([]);
    let plan = plan_restore(&source, &live).unwrap();
    let mut fake = fake(live).await;
    let ids = apply_restore_plan(&mut fake.api, &plan).await.unwrap();
    assert_eq!(fake.api.writes as usize, plan.operations.len());
    let state = fake.state.lock().unwrap();
    // The overwrite repair is the PATCH carrying permission_overwrites: the
    // category/channel creates now also carry the key (private at create,
    // TOG-9970 finding 1), so select by method, not mere key presence.
    let overwrite = state
        .writes
        .iter()
        .find(|(method, _, body)| method == "PATCH" && body.get("permission_overwrites").is_some())
        .unwrap();
    assert_eq!(
        overwrite.2["permission_overwrites"][0]["id"],
        ids.roles["r1"]
    );
    // Finding 1 pin: the channel create itself already restricts to the
    // saved overwrite set (resolved to the created role id), so the
    // channel is never initially public with a later repair.
    let created_alpha = state
        .writes
        .iter()
        .find(|(method, path, body)| {
            method == "POST"
                && path == "/guilds/g/channels"
                && body.get("name").and_then(Value::as_str) == Some("alpha")
        })
        .map(|(_, _, body)| body)
        .unwrap();
    assert_eq!(
        created_alpha["permission_overwrites"][0]["id"],
        ids.roles["r1"]
    );
    assert_eq!(
        state.snapshot["guild"]["system_channel_id"],
        ids.channels["c1"]
    );
    assert_eq!(state.snapshot["emojis"][0]["roles"][0], ids.roles["r1"]);
}

#[tokio::test]
async fn unsupported_create_capture_refuses_before_an_earlier_valid_patch() {
    let mut source = snapshot();
    source["roles"]
        .as_array_mut()
        .unwrap()
        .push(role("r2", "New", 2, false));
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().pop();
    live["roles"][1]["permissions"] = json!("1");
    let mut plan = plan_restore(&source, &live).unwrap();
    assert_eq!(plan.operations[0].label, "patch role Member");
    assert_eq!(plan.operations[1].label, "create role New");
    plan.operations[1].path = RestorePath::Literal("/guilds/g/emojis".into());
    let mut fake = fake(live).await;
    assert!(apply_restore_plan(&mut fake.api, &plan)
        .await
        .unwrap_err()
        .0
        .contains("Invalid"));
    assert_eq!(fake.api.writes, 0);
    assert!(fake.state.lock().unwrap().writes.is_empty());
}

#[tokio::test]
async fn malformed_or_unsupported_public_references_refuse_before_any_write() {
    let source = snapshot();
    let mut live = source.clone();
    live["roles"][1]["permissions"] = json!("1");
    for reference in [
        json!({"restoreReference": "role"}),
        json!({"restoreReference": "emoji", "sourceId": "e1"}),
    ] {
        let mut plan = plan_restore(&source, &live).unwrap();
        plan.operations.push(RestoreOperation {
            label: "bad reference".into(),
            method: "PATCH".into(),
            path: RestorePath::Literal("/guilds/g".into()),
            body: json!({"system_channel_id": reference}),
            capture_id: None,
        });
        let mut fake = fake(live.clone()).await;
        assert!(apply_restore_plan(&mut fake.api, &plan).await.is_err());
        assert_eq!(fake.api.writes, 0);
        assert!(fake.state.lock().unwrap().writes.is_empty());
    }
}

#[tokio::test]
async fn same_named_new_role_positions_are_checked_individually() {
    let mut source = with_authority(snapshot());
    source["roles"].as_array_mut().unwrap().extend([
        role("new-low", "Same", 2, false),
        role("new-high", "Same", 11, false),
    ]);
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().truncate(4);
    let plan = plan_restore(&source, &live).unwrap();
    let fake = fake(live.clone()).await;
    assert!(fake
        .api
        .assert_restore_permissions(&live, &plan)
        .await
        .unwrap_err()
        .to_string()
        .contains("planned position 11"));
    assert_eq!(fake.api.writes, 0);
}

#[tokio::test]
async fn forward_reference_in_public_plan_refuses_before_any_write() {
    let source = snapshot();
    let mut live = source.clone();
    live["roles"].as_array_mut().unwrap().remove(1);
    let mut plan = plan_restore(&source, &live).unwrap();
    assert_eq!(plan.operations[0].label, "create role Member");
    assert_eq!(plan.operations[1].label, "restore role positions");
    plan.operations.swap(0, 1);
    let mut fake = fake(live).await;
    assert!(apply_restore_plan(&mut fake.api, &plan)
        .await
        .unwrap_err()
        .0
        .contains("r1"));
    assert_eq!(fake.api.writes, 0);
    assert!(fake.state.lock().unwrap().writes.is_empty());
}
