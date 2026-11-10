//! Protected acceptance: channel-only creator settings commands are private reads.
//! Exercise Twilight gateway decoding and the production VoiceResponder event
//! sink. Only persistence and Discord I/O use the established observable doubles.
//! Builder: preserve this file; corrections belong to the test author.
#![cfg(test)]

use super::*;
use serde::de::DeserializeSeed;
use serde_json::Value;
use twilight_model::gateway::event::{GatewayEvent, GatewayEventDeserializer};

const OTHER_CREATOR: u64 = 201;
const UNKNOWN_CHANNEL: u64 = 202;
const POSITION: &str = "New rooms from <#200> go below, numbering from 17. Applies to rooms created from now on; open rooms stay where they are.";
const OTHER_POSITION: &str = "New rooms from <#201> go above, numbering from 4. Applies to rooms created from now on; open rooms stay where they are.";
const GROUP: &str = "Shared category numbering is on for <#200>: new rooms join the category's room block. Applies to rooms created from now on.";
const OTHER_GROUP: &str = "Shared category numbering is off for <#201>. New rooms number per creator. Applies to rooms created from now on.";
const TEXT: &str = "Companion text channels are on for <#200>: named `strategy-chat`, visible to occupants, admins and <@&42>. Applies to rooms created from now on; open rooms keep their settings.";
const OTHER_TEXT: &str = "Companion text channels are off for <#201>. Rooms already open keep theirs; new rooms get none.";
const NOT_CREATOR: &str = "That channel is not a voice-room creator. Pick one made with /create.";

fn saved_creators() -> Vec<CreatorChannel> {
    let mut first = CreatorChannel::new(GUILD, CREATOR);
    first.position = RoomPosition::Below;
    first.first_room_number = 17;
    first.group_by_category = true;
    first.text_channels = true;
    first.text_channel_name = Some("strategy-chat".to_owned());
    first.text_viewer_role_id = Some(42);
    first.name_template = "Squad ##".to_owned();
    first.default_limit = Some(7);
    first.private_default = true;
    let mut second = CreatorChannel::new(GUILD, OTHER_CREATOR);
    second.first_room_number = 4;
    second.text_channel_name = Some("retained-while-off".to_owned());
    second.text_viewer_role_id = Some(GUILD);
    vec![first, second]
}

struct Fixture {
    runtime: Arc<VoiceRuntime<Store, Http>>,
    replies: Arc<Replies>,
    trace: Trace,
    creators: Arc<Mutex<Vec<CreatorChannel>>>,
}

impl Fixture {
    fn new(creators: Vec<CreatorChannel>) -> Self {
        let trace = Trace::default();
        let creators = Arc::new(Mutex::new(creators));
        let shared_creators = Arc::clone(&creators);
        let shared_trace = Arc::clone(&trace);
        let runtime = Arc::new(VoiceRuntime::new(
            move || {
                let mut store = Store::new(Arc::clone(&shared_trace));
                store.creators = Arc::clone(&shared_creators);
                (store, Http::new(Arc::clone(&shared_trace)))
            },
            Duration::from_secs(60),
            true,
        ));
        let mut guild = command_snapshot();
        guild
            .channels
            .push(channel(OTHER_CREATOR, 2, Some(CATEGORY)));
        guild
            .channels
            .push(channel(UNKNOWN_CHANNEL, 2, Some(CATEGORY)));
        assert!(runtime.publish_snapshot(GUILD, guild));
        Self {
            runtime,
            replies: Arc::new(Replies::new(Arc::clone(&trace))),
            trace,
            creators,
        }
    }

    fn rows(&self) -> Vec<CreatorChannel> {
        let mut rows = self.creators.lock().unwrap().clone();
        rows.sort_by_key(|row| row.channel_id);
        rows
    }

    fn write_count(&self) -> usize {
        self.trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("add_creator:"))
            .count()
    }

    async fn dispatch(
        &self,
        name: &str,
        channel_id: u64,
        permissions: Permissions,
        changes: Vec<Value>,
    ) -> InteractionResponse {
        let before = self.replies.completed.lock().unwrap().len();
        let mut options = vec![json!({"name":"channel", "type":7, "value":channel_id.to_string()})];
        options.extend(changes);
        let packet = json!({
            "op":0, "s":before + 1, "t":"INTERACTION_CREATE",
            "d":{
                "application_id":"1", "id":(7000 + before).to_string(),
                "token":"creator-settings-fixture", "type":2, "version":1,
                "guild_id":GUILD.to_string(), "entitlements":[],
                "authorizing_integration_owners":{"0":GUILD.to_string()},
                "member":{
                    "permissions":permissions.bits().to_string(),
                    "roles":[(10_000 + permissions.bits()).to_string()],
                    "deaf":false, "mute":false, "flags":0,
                    "user":{"id":MEMBER.to_string(), "username":"staff", "discriminator":"0"}
                },
                "data":{"id":"3", "name":name, "type":1, "options":options}
            }
        })
        .to_string();
        let gateway = GatewayEventDeserializer::from_json(&packet)
            .expect("valid gateway envelope")
            .deserialize(&mut serde_json::Deserializer::from_str(&packet))
            .expect("Twilight decodes the gateway packet");
        let GatewayEvent::Dispatch(_, dispatch) = gateway else {
            panic!("expected gateway dispatch");
        };
        let sink = VoiceResponder::new(Arc::clone(&self.runtime), Arc::clone(&self.replies));
        sink.handle(&Event::from(dispatch), &DefaultInMemoryCache::new());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self.replies.completed.lock().unwrap().len() > before {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("gateway settings command must reply");
        let completed = self.replies.completed.lock().unwrap();
        assert_eq!(completed.len(), before + 1, "one completion per packet");
        let response = completed[before].clone();
        assert_eq!(
            response.data.as_ref().and_then(|data| data.flags),
            Some(MessageFlags::EPHEMERAL),
            "settings and refusals must remain private"
        );
        response
    }
}

async fn assert_read(name: &str, expected: &str, other_expected: &str) {
    let original = saved_creators();
    let fixture = Fixture::new(original.clone());
    for (channel_id, text) in [
        (CREATOR, expected),
        (OTHER_CREATOR, other_expected),
        (CREATOR, expected),
    ] {
        let response = fixture
            .dispatch(name, channel_id, Permissions::MANAGE_CHANNELS, vec![])
            .await;
        // Check the write oracle first: /textchannels already returns a summary
        // on baseline, but rewriting an unchanged row is not a read.
        assert_eq!(
            fixture.write_count(),
            0,
            "/{name} channel-only must never save"
        );
        assert_eq!(
            fixture.rows(),
            original,
            "/{name} must preserve every creator field"
        );
        assert_eq!(
            response_text(&response),
            text,
            "/{name} reads the selected saved creator"
        );
    }
}

#[tokio::test]
async fn position_channel_only_reads_saved_placement_without_writing() {
    assert_read("position", POSITION, OTHER_POSITION).await;
}

#[tokio::test]
async fn group_channel_only_reads_saved_grouping_without_writing() {
    assert_read("group", GROUP, OTHER_GROUP).await;
}

#[tokio::test]
async fn textchannels_channel_only_reads_saved_companions_without_writing() {
    assert_read("textchannels", TEXT, OTHER_TEXT).await;
}

#[tokio::test]
async fn textchannels_read_uses_default_name_and_private_viewers_without_saving() {
    let mut original = saved_creators();
    original[0].text_channel_name = None;
    original[0].text_viewer_role_id = None;
    let fixture = Fixture::new(original.clone());
    let response = fixture
        .dispatch(
            "textchannels",
            CREATOR,
            Permissions::MANAGE_CHANNELS,
            vec![],
        )
        .await;
    assert_eq!(fixture.write_count(), 0);
    assert_eq!(fixture.rows(), original);
    assert_eq!(response_text(&response), "Companion text channels are on for <#200>: named `voice-chat`, visible to occupants and admins. Applies to rooms created from now on; open rooms keep their settings.");
}

#[tokio::test]
async fn textchannels_read_reports_everyone_without_saving() {
    let mut original = saved_creators();
    original[0].text_viewer_role_id = Some(GUILD);
    let fixture = Fixture::new(original.clone());
    let response = fixture
        .dispatch(
            "textchannels",
            CREATOR,
            Permissions::MANAGE_CHANNELS,
            vec![],
        )
        .await;
    assert_eq!(fixture.write_count(), 0);
    assert_eq!(fixture.rows(), original);
    assert_eq!(response_text(&response), "Companion text channels are on for <#200>: named `strategy-chat`, visible to everyone. Applies to rooms created from now on; open rooms keep their settings.");
}

async fn assert_save(name: &str, changes: Vec<Value>, changed: CreatorChannel, text: &str) {
    let fixture = Fixture::new(saved_creators());
    let response = fixture
        .dispatch(name, CREATOR, Permissions::MANAGE_CHANNELS, changes)
        .await;
    assert_eq!(
        fixture.write_count(),
        1,
        "/{name} with a setting still saves exactly once"
    );
    assert_eq!(
        fixture.rows(),
        vec![changed, saved_creators()[1].clone()],
        "only requested fields of the selected creator change"
    );
    assert_eq!(response_text(&response), text);
}

#[tokio::test]
async fn control_position_side_only_saves_and_keeps_numbering() {
    let mut changed = saved_creators()[0].clone();
    changed.position = RoomPosition::Above;
    assert_save("position", vec![json!({"name":"position", "type":3, "value":"above"})], changed, "New rooms from <#200> go above, numbering from 17. Applies to rooms created from now on; open rooms stay where they are.").await;
}

#[tokio::test]
async fn control_position_number_only_saves_and_keeps_side() {
    let mut changed = saved_creators()[0].clone();
    changed.first_room_number = 9;
    assert_save("position", vec![json!({"name":"first-number", "type":4, "value":9})], changed, "New rooms from <#200> go below, numbering from 9. Applies to rooms created from now on; open rooms stay where they are.").await;
}

#[tokio::test]
async fn control_group_false_is_a_save_not_an_omitted_toggle() {
    let mut changed = saved_creators()[0].clone();
    changed.group_by_category = false;
    assert_save("group", vec![json!({"name":"enabled", "type":5, "value":false})], changed, "Shared category numbering is off for <#200>. New rooms number per creator. Applies to rooms created from now on.").await;
}

#[tokio::test]
async fn control_group_true_saves_and_summarizes() {
    let mut original = saved_creators();
    original[0].group_by_category = false;
    let fixture = Fixture::new(original);
    let response = fixture
        .dispatch(
            "group",
            CREATOR,
            Permissions::MANAGE_CHANNELS,
            vec![json!({"name":"enabled", "type":5, "value":true})],
        )
        .await;
    assert_eq!(fixture.write_count(), 1);
    assert_eq!(fixture.rows(), saved_creators());
    assert_eq!(response_text(&response), GROUP);
}

#[tokio::test]
async fn control_textchannels_false_saves_and_preserves_name_and_role() {
    let mut changed = saved_creators()[0].clone();
    changed.text_channels = false;
    assert_save("textchannels", vec![json!({"name":"enabled", "type":5, "value":false})], changed, "Companion text channels are off for <#200>. Rooms already open keep theirs; new rooms get none.").await;
}

#[tokio::test]
async fn control_textchannels_name_only_saves_without_disabling_companions() {
    let mut changed = saved_creators()[0].clone();
    changed.text_channel_name = Some("lounge".to_owned());
    assert_save("textchannels", vec![json!({"name":"name", "type":3, "value":"  lounge  "})], changed, "Companion text channels are on for <#200>: named `lounge`, visible to occupants, admins and <@&42>. Applies to rooms created from now on; open rooms keep their settings.").await;
}

#[tokio::test]
async fn control_textchannels_viewer_only_saves_without_changing_name_or_toggle() {
    let mut changed = saved_creators()[0].clone();
    changed.text_viewer_role_id = Some(GUILD);
    assert_save("textchannels", vec![json!({"name":"viewer-role", "type":8, "value":GUILD.to_string()})], changed, "Companion text channels are on for <#200>: named `strategy-chat`, visible to everyone. Applies to rooms created from now on; open rooms keep their settings.").await;
}

#[tokio::test]
async fn control_non_creator_refuses_reads_and_edits_without_writing() {
    for (name, changes) in [
        (
            "position",
            vec![json!({"name":"first-number", "type":4, "value":9})],
        ),
        (
            "group",
            vec![json!({"name":"enabled", "type":5, "value":false})],
        ),
        (
            "textchannels",
            vec![json!({"name":"name", "type":3, "value":"lounge"})],
        ),
    ] {
        let original = saved_creators();
        let fixture = Fixture::new(original.clone());
        for options in [vec![], changes] {
            let response = fixture
                .dispatch(name, UNKNOWN_CHANNEL, Permissions::MANAGE_CHANNELS, options)
                .await;
            assert_eq!(response_text(&response), NOT_CREATOR, "/{name}");
            assert_eq!(fixture.write_count(), 0);
            assert_eq!(fixture.rows(), original);
        }
    }
}

#[tokio::test]
async fn control_no_manage_channels_refuses_reads_and_edits_without_disclosing_settings() {
    for (name, changes) in [
        (
            "position",
            vec![json!({"name":"first-number", "type":4, "value":9})],
        ),
        (
            "group",
            vec![json!({"name":"enabled", "type":5, "value":false})],
        ),
        (
            "textchannels",
            vec![json!({"name":"name", "type":3, "value":"lounge"})],
        ),
    ] {
        let original = saved_creators();
        let fixture = Fixture::new(original.clone());
        for options in [vec![], changes] {
            let response = fixture
                .dispatch(name, CREATOR, Permissions::VIEW_CHANNEL, options)
                .await;
            assert_eq!(
                response_text(&response),
                format!("You need Manage Channels to use /{name}.")
            );
            assert_eq!(fixture.write_count(), 0);
            assert_eq!(fixture.rows(), original);
        }
    }
}
