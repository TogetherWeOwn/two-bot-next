//! Protected feed lifecycle copy: a supplied ID must not escape trusted framing.

use two_bot_core::feeds::{
    feed_already_paused_text, feed_already_resumed_text, feed_paused_text, feed_resumed_text,
};

type Renderer = fn(&str) -> String;

const REPLIES: [(Renderer, &str); 4] = [
    (feed_paused_text, "Feed relay paused: "),
    (feed_already_paused_text, "Feed relay is already paused: "),
    (feed_resumed_text, "Feed relay resumed: "),
    (feed_already_resumed_text, "Feed relay is already enabled: "),
];

fn assert_hostile_id_receipts(render: Renderer, heading: &str) {
    for (id, escaped, stripped) in [
        (
            "relay` **forged**",
            "relay\\` \\*\\*forged\\*\\*",
            "relay forged",
        ),
        (
            "relay```\n**forged**",
            "relay\\`\\`\\` \\*\\*forged\\*\\*",
            "relay forged",
        ),
        ("relay\r\nnext", "relay  next", "relay  next"),
        (
            "relay[forged](https://example.org)",
            "relay\\[forged\\](https://example.org)",
            "relayforgedhttps://example.org",
        ),
        (
            "relay`<@&8888> @everyone",
            "relay\\`<@&8888> @everyone",
            "relay@&8888 @everyone",
        ),
    ] {
        let text = render(id);
        // Backslash escapes do NOT protect a delimiter inside inline code.
        // Accept inspect-style escaping outside code OR inert text inside it.
        let accepted = [
            format!("{heading}{escaped}."),
            format!("{heading}`{stripped}`."),
        ];
        assert!(
            accepted.contains(&text),
            "active or unreadable feed receipt: {text:?}"
        );
        assert!(text.encode_utf16().count() <= 2000);
    }
}

#[test]
fn paused_receipt_renders_hostile_ids_inert() {
    assert_hostile_id_receipts(feed_paused_text, "Feed relay paused: ");
}

#[test]
fn already_paused_receipt_renders_hostile_ids_inert() {
    assert_hostile_id_receipts(feed_already_paused_text, "Feed relay is already paused: ");
}

#[test]
fn resumed_receipt_renders_hostile_ids_inert() {
    assert_hostile_id_receipts(feed_resumed_text, "Feed relay resumed: ");
}

#[test]
fn already_enabled_receipt_renders_hostile_ids_inert() {
    assert_hostile_id_receipts(feed_already_resumed_text, "Feed relay is already enabled: ");
}

#[test]
fn ordinary_stable_ids_keep_the_existing_copy_byte_for_byte() {
    for id in [
        "relay-123",
        "feed_9",
        "8fdb24fa-3376-4dac-a940-34fec450ed6c",
    ] {
        for (render, heading) in REPLIES {
            assert_eq!(render(id), format!("{heading}`{id}`."));
        }
    }
}

#[test]
fn maximum_admitted_multibyte_id_stays_bounded_and_readable() {
    // The existing command ingress admits at most 128 UTF-16 units.
    let id = "😀".repeat(64);
    for (render, heading) in REPLIES {
        let text = render(&id);
        assert_eq!(text, format!("{heading}`{id}`."));
        assert!(text.encode_utf16().count() <= 2000);
    }
}
