use serde_json::{json, Value};
use two_bot_discord::brand::{ephemeral_reply_embed, EmbedKind};

/// Accept only the legacy text edit or the exact event embed edit.
pub fn deferred_text(body: &Value, kind: EmbedKind) -> &str {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(body["allowed_mentions"]["users"], json!([]));
    assert_eq!(body["allowed_mentions"]["roles"], json!([]));
    let content = body["content"].as_str().expect("reply content");
    if content.is_empty() {
        let description = body["embeds"][0]["description"]
            .as_str()
            .expect("embed description");
        let expected = ephemeral_reply_embed(kind, "Events", description.to_owned());
        assert_eq!(body["embeds"], json!([expected]));
        description
    } else {
        assert!(body["embeds"].is_null(), "text reply has no embeds");
        content
    }
}

#[test]
fn deferred_reply_rejects_noncanonical_or_unsafe_shapes() {
    let mentions = json!({"parse": [], "users": [], "roles": []});
    let embed = ephemeral_reply_embed(EmbedKind::Danger, "Events", "refused".to_owned());
    let plain = json!({"content": "refused", "allowed_mentions": mentions});
    let rich = json!({"content": "", "embeds": [embed], "allowed_mentions": mentions});
    for body in [&plain, &rich] {
        assert_eq!(deferred_text(body, EmbedKind::Danger), "refused");
    }
    let mut wrong_copy = rich.clone();
    wrong_copy["embeds"][0]["description"] = json!("wrong");
    assert!(std::panic::catch_unwind(|| {
        assert_eq!(deferred_text(&wrong_copy, EmbedKind::Danger), "refused");
    })
    .is_err());
    let mut wrong_color = rich.clone();
    wrong_color["embeds"][0]["color"] = json!(0);
    let mut extra = rich.clone();
    extra["embeds"].as_array_mut().unwrap().push(json!({}));
    let mut unsafe_mentions = rich.clone();
    unsafe_mentions["allowed_mentions"]["users"] = json!(["123"]);
    let mut mixed = plain.clone();
    mixed["embeds"] = rich["embeds"].clone();
    for body in [wrong_color, extra, unsafe_mentions, mixed] {
        assert!(std::panic::catch_unwind(|| deferred_text(&body, EmbedKind::Danger)).is_err());
    }
}
