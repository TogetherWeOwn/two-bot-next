#![cfg(test)]

use super::*;

#[derive(Clone, Copy, Debug)]
enum Adapter {
    Room,
    Companion,
    Join,
}

impl Adapter {
    fn kind(self) -> u8 {
        match self {
            Self::Companion => 0,
            Self::Room | Self::Join => 2,
        }
    }

    async fn create(self, api: &RoomHttp, parent: Option<u64>) -> Result<Channel, RoomHttpError> {
        match self {
            Self::Room => {
                let mut attributes = attributes();
                attributes.parent_id = parent;
                api.create_room(100, "room", &attributes, || true).await
            }
            Self::Companion => {
                let plan = TextChannelPlan {
                    room_id: 500,
                    guild_id: 100,
                    name: "companion".to_owned(),
                    category_id: 400,
                    overwrites: vec![],
                    settings: two_bot_core::voice_text_channel::TextChannelSettings {
                        enabled: true,
                        configured_name: None,
                        viewer_role_id: None,
                    },
                };
                api.create_companion(&plan, 999, || true).await
            }
            Self::Join => {
                api.create_join_voice_channel(100, "join", parent, None, || true)
                    .await
            }
        }
    }
}

#[tokio::test]
async fn malformed_identities_and_bodyless_success_are_uncertain_for_every_adapter() {
    for adapter in [Adapter::Room, Adapter::Companion, Adapter::Join] {
        let valid =
            json!({"id": "600", "guild_id": "100", "type": adapter.kind(), "parent_id": "400"});
        let mut bodies = vec![Value::Null, json!({}), json!([]), json!("not a channel")];
        for field in ["id", "guild_id", "type", "parent_id"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            bodies.push(missing);
            for value in [Value::Null, json!("invalid"), json!({}), json!([])] {
                let mut malformed = valid.clone();
                malformed[field] = value;
                bodies.push(malformed);
            }
        }
        for field in ["id", "guild_id", "parent_id"] {
            let mut zero = valid.clone();
            zero[field] = json!("0");
            bodies.push(zero);
        }
        let mut unknown_kind = valid;
        unknown_kind["type"] = json!(255);
        bodies.push(unknown_kind);
        for body in bodies {
            let mock = Mock::start(vec![response(201, body.clone())]).await;
            assert_eq!(
                adapter.create(&mock.api, Some(400)).await,
                Err(RoomHttpError::UnknownOutcome),
                "{adapter:?}: {body}"
            );
            let requests = mock.state.recorded.lock().unwrap();
            assert_eq!(requests.len(), 1, "no retry for {adapter:?}: {body}");
            assert_eq!(requests[0].method, Method::POST);
        }
        let mock = Mock::start(vec![response(204, Value::Null)]).await;
        assert_eq!(
            adapter.create(&mock.api, Some(400)).await,
            Err(RoomHttpError::UnknownOutcome)
        );
        assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn optional_parent_accepts_absence_or_null_but_not_an_unrequested_category() {
    for adapter in [Adapter::Room, Adapter::Join] {
        for parent in [None, Some(Value::Null), Some(json!("400"))] {
            let mut body = json!({"id": "600", "guild_id": "100", "type": adapter.kind()});
            if let Some(parent) = &parent {
                body["parent_id"] = parent.clone();
            }
            let mock = Mock::start(vec![response(201, body)]).await;
            let result = adapter.create(&mock.api, None).await;
            if parent == Some(json!("400")) {
                assert_eq!(result, Err(RoomHttpError::UnknownOutcome));
            } else {
                assert_eq!(result.unwrap().id.get(), 600);
            }
            assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn invalid_json_is_an_uncertain_creation_receipt() {
    assert_eq!(
        creation_receipt(b"{", 100, ChannelType::GuildVoice, Some(400)),
        Err(RoomHttpError::UnknownOutcome)
    );
}
