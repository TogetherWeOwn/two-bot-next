use std::net::IpAddr;
use two_bot_core::feeds::*;
use two_bot_core::feeds_http::*;

#[test]
fn poll_schedule_is_immediate_300_seconds_by_default_and_never_overlaps() {
    let gates = two_bot_core::FeatureGates::from_map(&std::collections::HashMap::new()).unwrap();
    assert_eq!(gates.feed_poll_seconds, 300);
    let mut schedule = FeedPollSchedule::new(gates.feed_poll_seconds).unwrap();
    assert!(schedule.begin(1000));
    assert!(!schedule.begin(301_000)); // earlier pass still running
    schedule.finish();
    assert!(!schedule.begin(300_999));
    assert!(schedule.begin(301_000));
    schedule.finish(); // failure uses the same finish path
    assert!(!schedule.begin(301_001));
    assert!(schedule.begin(601_000));
    assert!(FeedPollSchedule::new(59).is_err());
    assert!(FeedPollSchedule::new(86_401).is_err());
}

fn ip(address: &str) -> IpAddr {
    address.parse().unwrap()
}

fn relay(kind: FeedKind) -> FeedRelay {
    FeedRelay {
        id: "feed-1".into(),
        guild_id: "guild-1".into(),
        channel_id: "channel-1".into(),
        kind,
        source: "https://example.org/feed.xml".into(),
        enabled: true,
        last_checked_at: None,
        created_by: "actor".into(),
        created_at: 0,
        updated_at: 0,
    }
}

fn item(key: &str) -> FeedItem {
    FeedItem {
        key: key.into(),
        title: "Hello".into(),
        url: "https://example.org/post".into(),
        published_at: None,
    }
}

#[test]
fn refuses_nonpublic_ipv4_and_ipv6_including_translation_ranges() {
    for address in [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.0.0.1",
        "192.168.0.1",
        "192.0.2.1",
        "192.88.99.1",
        "198.18.1.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:8.8.8.8",
        "::ffff:127.0.0.1",
        "::127.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "64:ff9b:1::a00:1",
        "100::1",
        "100:0:0:1::1",
        "2001::1",
        "2001:2::1",
        "2001:db8::1",
        "2002:7f00:1::",
        "3fff::1",
        "5f00::1",
        "fc00::1",
        "fd00::1",
        "fe80::1",
        "ff02::1",
        "4000::1",
    ] {
        assert!(!is_public_address(ip(address)), "allowed {address}");
    }
    for address in [
        "8.8.8.8",
        "1.1.1.1",
        "172.32.0.1",
        "100.128.0.1",
        "2001:4860:4860::8888",
        "2606:4700:4700::1111",
    ] {
        assert!(is_public_address(ip(address)), "refused {address}");
    }
}

#[test]
fn sources_reject_credentials_and_canonicalized_private_literals() {
    for source in [
        "http://example.org/",
        "file:///etc/passwd",
        "https://user:pass@example.org/",
        "https://localhost/",
        "https://LOCALHOST./",
        "https://127.1/",
        "https://2130706433/",
        "https://0x7f000001/",
        "https://[::1]/",
        "https://[::ffff:7f00:1]/",
        "https://10.1.1.1/",
    ] {
        assert!(validate_source(source).is_err(), "accepted {source}");
    }
    assert_eq!(
        normalize_source(FeedKind::Youtube, "UCabcdefghijklmnopqrstuv").unwrap(),
        "https://www.youtube.com/feeds/videos.xml?channel_id=UCabcdefghijklmnopqrstuv"
    );
    assert!(normalize_source(FeedKind::Twitch, "https://example.org/twitch.xml").is_ok());
    assert!(normalize_source(FeedKind::Twitch, "streamer-name").is_err());
}

#[test]
fn dns_results_are_all_or_nothing_and_socket_targets_are_pinned() {
    let url = validate_source("https://example.org/feed").unwrap();
    let plan = PublicRequest::prepare(url.clone(), &[ip("8.8.8.8"), ip("1.1.1.1")]).unwrap();
    assert_eq!(plan.url(), &url);
    assert_eq!(plan.addresses()[0].ip(), ip("8.8.8.8"));
    assert_eq!(plan.addresses()[0].port(), 443);
    assert!(PublicRequest::prepare(url.clone(), &[]).is_err());
    assert!(PublicRequest::prepare(url.clone(), &[ip("8.8.8.8"), ip("10.0.0.1")]).is_err());
    // A new DNS lookup at a redirect/request boundary refuses a rebound host.
    assert!(PublicRequest::prepare(url, &[ip("127.0.0.1")]).is_err());
    assert_eq!(plan.addresses()[0].ip(), ip("8.8.8.8"));
    let literal = validate_source("https://8.8.8.8:8443/feed").unwrap();
    assert!(PublicRequest::prepare(literal.clone(), &[ip("1.1.1.1")]).is_err());
    assert_eq!(
        PublicRequest::prepare(literal, &[ip("8.8.8.8")])
            .unwrap()
            .addresses()[0]
            .port(),
        8443
    );
}

#[test]
fn redirects_refuse_private_cross_host_downgrade_credentials_and_overflow() {
    let url = validate_source("https://example.org/old").unwrap();
    for target in [
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://10.0.0.1/",
        "//private.example/",
        "https://www.example.org/",
        "http://example.org/",
        "file:///etc/passwd",
        "https://user@example.org/",
        "",
    ] {
        assert!(
            redirect_target(&url, target, 0).is_err(),
            "accepted {target}"
        );
    }
    let target = redirect_target(&url, "/new", 2).unwrap();
    assert_eq!(target.as_str(), "https://example.org/new");
    assert!(PublicRequest::prepare(target, &[ip("10.0.0.1")]).is_err());
    assert!(redirect_target(&url, "/new", MAX_REDIRECT_HOPS).is_err());
    assert_eq!(FEED_USER_AGENT, "Owen/1.0 (+https://two.gg)");
}

#[test]
fn body_cap_counts_chunked_decompressed_bytes_and_utf8() {
    assert!(LimitedBody::new(Some(MAX_FEED_BYTES as u64 + 1)).is_err());
    let mut body = LimitedBody::new(None).unwrap();
    body.push(&vec![b'a'; MAX_FEED_BYTES]).unwrap();
    assert!(body.push(b"b").is_err());
    assert_eq!(body.finish().unwrap().len(), MAX_FEED_BYTES);
    let mut body = LimitedBody::new(Some(1)).unwrap();
    assert!(body.push(&vec![b'a'; MAX_FEED_BYTES + 1]).is_err());
    let mut body = LimitedBody::default();
    body.push(&[0xe2]).unwrap();
    body.push(&[0x82, 0xac]).unwrap();
    assert_eq!(body.finish().unwrap(), "€");
    let mut body = LimitedBody::default();
    body.push(&[0xff]).unwrap();
    assert!(body.finish().is_err());
    assert!(validate_content_type(Some("application/atom+xml; charset=UTF-8")).is_ok());
    assert!(validate_content_type(None).is_ok());
    assert!(validate_content_type(Some("text/html")).is_err());
}

#[test]
fn rss_fixture_preserves_guid_entities_cdata_and_date() {
    let items = parse_xml_feed(include_str!("fixtures/feeds/rss.xml")).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key, "rss-2");
    assert_eq!(items[0].title, "Fish & chips");
    assert_eq!(items[0].url, "https://example.org/new?a=1&b=2");
    assert_eq!(
        items[0].published_at.as_deref(),
        Some("Wed, 30 Sep 2026 01:00:00 GMT")
    );
    assert_eq!(items[1].title, "Old <post>");
    assert_eq!(
        poll_candidates(&relay(FeedKind::Rss), &items)[0]
            .as_ref()
            .unwrap()
            .item_key,
        item_key(&items[1]).unwrap()
    );
}

#[test]
fn youtube_atom_fixture_chooses_alternate_not_self_link() {
    let items = parse_xml_feed(include_str!("fixtures/feeds/youtube.xml")).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "yt:video:123");
    assert_eq!(items[0].url, "https://www.youtube.com/watch?v=123");
    assert!(plan_post(&relay(FeedKind::Youtube), &items[0])
        .unwrap()
        .content
        .starts_with("New YouTube upload:"));
}

#[test]
fn twitch_is_legacy_xml_not_helix_api() {
    let items = parse_xml_feed(include_str!("fixtures/feeds/twitch.xml")).unwrap();
    assert_eq!(items[0].key, "twitch:stream:42");
    assert_eq!(items[0].url, "https://www.twitch.tv/example");
    assert!(plan_post(&relay(FeedKind::Twitch), &items[0])
        .unwrap()
        .content
        .starts_with("Twitch update:"));
}

#[test]
fn atom_link_resolution_matches_legacy_resolve_link() {
    // A mailto alternate must not shadow the HTTPS alternate that follows it.
    let xml = "<feed><entry><id>x</id><title>t</title>\
        <link rel=\"alternate\" href=\"mailto:owner@example.org\"/>\
        <link rel=\"alternate\" href=\"https://example.org/post\"/>\
        </entry></feed>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url, "https://example.org/post");

    // An attribute-bearing link without href contributes nothing (legacy
    // only consults record.href, never the text body); resolution falls
    // through to the bare text link.
    let xml = "<feed><entry><id>x</id><title>t</title>\
        <link rel=\"alternate\" type=\"text/html\">https://example.org/shadowed</link>\
        <link>https://example.org/plain</link>\
        </entry></feed>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url, "https://example.org/plain");

    // An empty href is ignored (legacy filters falsy hrefs before
    // considering hrefs[0]), so the nonempty candidates decide.
    let xml = "<feed><entry><id>x</id><title>t</title>\
        <link rel=\"alternate\" href=\"\"/>\
        <link rel=\"alternate\" href=\"javascript:alert(1)\"/>\
        <link>https://example.org/plain</link>\
        </entry></feed>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url, "https://example.org/plain");
}

#[test]
fn structured_guid_falls_through_to_id_like_legacy_text_value() {
    // Legacy textValue only reads the record's own #text: nested markup in
    // <guid> contributes its content but never shadows own text, so a
    // mixed guid keeps "prepost" and falls through only when fully empty.
    let xml = "<rss><channel><item>\
        <guid><value>nested</value></guid><link>https://example.org/post</link>\
        </item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "https://example.org/post");
    let xml = "<rss><channel><item>\
        <guid>pre<value>nested</value>post</guid>\
        <link>https://example.org/post</link>\
        </item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "prepost");
    // Sanity: a plain-text guid still wins over id and link.
    let xml_plain = "<rss><channel><item>\
        <guid>guid-key</guid><id>id-key</id>\
        <link>https://example.org/post</link>\
        </item></channel></rss>";
    assert_eq!(parse_xml_feed(xml_plain).unwrap()[0].key, "guid-key");
}

#[test]
fn numeric_references_keep_legacy_identity_for_delivery_dedupe() {
    // Legacy parses with processEntities:false and only expands the five
    // named escapes afterwards, so `<guid>post&#49;</guid>` keeps the raw
    // `post&#49;` key (sha256 0a1314…). Decoding it to `post1` (0c99c0…)
    // would bypass restored delivery dedupe and repost. Numeric and hex
    // references survive everywhere; chained named escapes still decode
    // exactly like legacy decodeXml (a&amp;lt;b -> a<b).
    let xml = "<rss><channel><item>\
        <guid>post&#49;</guid><title>a&amp;lt;b</title>\
        <link>https://example.org/?a=1&amp;b=2</link>\
        </item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "post&#49;");
    assert_eq!(items[0].title, "a<b");
    assert_eq!(items[0].url, "https://example.org/?a=1&b=2");
    assert_eq!(
        item_key(&items[0]).unwrap(),
        "0a13142831f76095730df8bee6c6961d78aa5e86b67f41e20dc5b95eb030040e"
    );
    // A restored legacy ledger row with this hash must match the fresh
    // parse, so recovery reconciles instead of reposting.
    let legacy = FeedItem {
        key: "post&#49;".into(),
        title: "a<b".into(),
        url: "https://example.org/?a=1&b=2".into(),
        published_at: None,
    };
    assert_eq!(item_key(&legacy).unwrap(), item_key(&items[0]).unwrap());
    assert_eq!(
        delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
        delivery_nonce("feed-1", &item_key(&legacy).unwrap())
    );
    // Hex references and CDATA bodies also pass through untouched.
    let xml = "<rss><channel><item>\
        <guid>&#x41;&#65;</guid><title><![CDATA[cd&#50;]]></title>\
        <link>https://example.org/post</link>\
        </item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "&#x41;&#65;");
    assert_eq!(items[0].title, "cd&#50;");
}

#[test]
fn unicode_trim_matches_legacy_string_trim_for_delivery_identity() {
    // Legacy trims with JavaScript String.trim(); Rust str::trim() differs
    // in exactly two code points (strips U+0085, keeps U+FEFF -- JS does
    // the opposite). Either divergence changes the hashed key/nonce and
    // bypasses restored delivery dedupe, reposting the item.
    // (\u{feff} / \u{85} escapes keep the invisible code points explicit.)
    let xml = "<rss><channel><item><guid>\u{feff}post1</guid>\
        <link>https://example.org/post</link></item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "post1");
    assert_eq!(
        item_key(&items[0]).unwrap(),
        "0c99c0ff97ab9d918039af1f390708ea1e0a32feed5c43469328fe2b559c9260"
    );
    assert_eq!(
        delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
        "d3de15aef4a7f86bb7aa4c56"
    );
    // U+0085 is NOT whitespace to legacy: it survives into the key.
    let xml = "<rss><channel><item><guid>\u{85}post1</guid>\
        <link>https://example.org/post</link></item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "\u{85}post1");
    assert_eq!(
        item_key(&items[0]).unwrap(),
        "ec7e0c317ea3b013b32527cbc2ed150be322af67d2f03e70320e9e186a3d9568"
    );
    assert_eq!(
        delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
        "02226b2bc943fadb600b482b"
    );
    // A guid of only trimmable padding is empty to legacy, so identity
    // falls through to the link exactly like a missing guid.
    let xml = "<rss><channel><item><guid>\u{feff} </guid>\
        <link>https://example.org/post</link></item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "https://example.org/post");
}

#[test]
fn prefixed_extension_fields_do_not_shadow_core_guid() {
    // Legacy fast-xml-parser keeps the `vendor:` prefix on property names,
    // so `row.guid` never sees `<vendor:guid>`. Selecting by local name
    // would hash `extension-key` instead of `real-key` and miss the
    // restored delivered row.
    let xml = "<rss xmlns:vendor=\"urn:vendor\"><channel><item>\
        <vendor:guid>extension-key</vendor:guid><guid>real-key</guid>\
        <link>https://example.org/post</link>\
        </item></channel></rss>";
    let items = parse_xml_feed(xml).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "real-key");
    assert_eq!(
        item_key(&items[0]).unwrap(),
        "820b4debdcadc0f01b263929238f1df85926a5e067b332f16ae52eabb1c1b42b"
    );
    assert_eq!(
        delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
        "46a5c3c8086f2dbb35b6c65b"
    );
}

#[test]
fn rejects_xml_entities_malformed_size_and_item_explosion() {
    for xml in ["<rss>", "<html/>",
        "<!DOCTYPE rss [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><rss><channel><item><title>&secret;</title></item></channel></rss>",
        "<!DOCTYPE rss [<!ENTITY a 'AAAA'><!ENTITY b '&a;&a;&a;'>]><rss><channel>&b;</channel></rss>"] {
        assert!(parse_xml_feed(xml).is_err());
    }
    assert!(parse_xml_feed(&"a".repeat(MAX_FEED_BYTES + 1)).is_err());
    let xml = format!(
        "<rss><channel>{}</channel></rss>",
        "<item/>".repeat(MAX_FEED_ITEMS + 1)
    );
    assert!(matches!(parse_xml_feed(&xml), Err(FeedError::TooManyItems)));
    let xml = "<rss><channel><item><guid>1</guid><link>javascript:alert(1)</link></item><item><guid>2</guid><link>https://user:pass@example.org</link></item></channel></rss>";
    assert!(parse_xml_feed(xml).unwrap().is_empty());
}

#[test]
fn stable_keys_nonces_budget_and_mentions_survive_restart() {
    let feed = relay(FeedKind::Rss);
    let post = plan_post(&feed, &item("rss-1")).unwrap();
    assert_eq!(post, plan_post(&feed, &item("rss-1")).unwrap());
    assert_eq!(post.item_key.len(), 64);
    assert_eq!(post.nonce.len(), 24);
    assert!(post.suppress_mentions && post.enforce_nonce);
    assert_ne!(post.nonce, delivery_nonce("other-feed", &post.item_key));
    assert!(item_key(&FeedItem {
        key: " ".into(),
        url: " ".into(),
        ..item("")
    })
    .is_err());
    assert_eq!(poll_candidates(&feed, &[item("x"), item("x")]).len(), 1);
    assert_eq!(
        delivery_action(DeliveryClaim::Fresh, 19),
        DeliveryAction::Send
    );
    assert_eq!(
        delivery_action(DeliveryClaim::Fresh, 20),
        DeliveryAction::Defer
    );
    assert_eq!(
        delivery_action(DeliveryClaim::Recovered, 0),
        DeliveryAction::ReconcileByNonce
    );
    let long = FeedItem {
        title: "😀".repeat(2000),
        ..item("x")
    };
    assert!(
        plan_post(&feed, &long)
            .unwrap()
            .content
            .encode_utf16()
            .count()
            <= 2000
    );
}

#[test]
fn commands_require_enabled_configured_guild_and_manage_guild() {
    let mut context = FeedCommandContext {
        enabled: false,
        configured_guild_id: "guild-1",
        guild_id: "guild-1",
        channel_id: "channel-1",
        actor_id: "actor",
        can_manage_guild: true,
        now_ms: 1234,
    };
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    context.enabled = true;
    context.can_manage_guild = false;
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::PermissionRequired)
    ));
    context.can_manage_guild = true;
    context.guild_id = "other";
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    context.guild_id = "guild-1";
    let FeedCommandPlan::Add(feed) = plan_command(
        &context,
        FeedCommand::Add {
            id: "feed-1".into(),
            kind: FeedKind::Rss,
            source: "https://example.org/feed.xml".into(),
        },
    )
    .unwrap() else {
        panic!("add plan");
    };
    assert_eq!(feed.guild_id, "guild-1");
    assert_eq!(feed.created_at, 1234);
    assert_eq!(feed_removed_text(false), "No feed relay with that id.");
    assert_eq!(feed_list_text(&[]), "No feed relays configured.");
    assert_eq!(
        feed_list_text(&[feed]),
        "`feed-1` rss → <#channel-1> https://example.org/feed.xml"
    );
}
