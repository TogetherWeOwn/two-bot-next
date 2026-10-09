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
        "https://example.org:8443/",
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
    assert!(validate_source("https://example.org:443/feed").is_ok());
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
    let literal = validate_source("https://8.8.8.8/feed").unwrap();
    assert!(PublicRequest::prepare(literal.clone(), &[ip("1.1.1.1")]).is_err());
    assert_eq!(
        PublicRequest::prepare(literal, &[ip("8.8.8.8")])
            .unwrap()
            .addresses()[0]
            .port(),
        443
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
fn legacy_parser_emulation_matches_identity_corpus() {
    // Full reviewer corpus ([TOG-10352](/TOG/issues/TOG-10352) verdicts on
    // `c44577e` and `3a45987`): legacy `parseXml` normalizes CRLF/CR to LF
    // before anything else — including inside attribute values — trims each
    // plain-text run on flush (CDATA flushes the pending run first, then
    // appends raw; comments glue without flushing), rejects repeated fields
    // as arrays, resolves structured link records through their `href` child
    // (a nested element/PI child with no `href` field discards the link
    // rather than falling back to its text), reads child `<rel>` through the
    // same attribute-over-child projection for alternate selection, and lets
    // entry attributes shadow same-named children. Every case pins the exact
    // legacy hash/nonce so a restored delivery row is found instead of
    // reposted.
    for (xml, key, url, hash, nonce) in [
        (
            "<rss><channel><item><guid>first\r\nsecond</guid><link>https://example.org/post</link></item></channel></rss>",
            "first\nsecond",
            "https://example.org/post",
            "4252f8d56b4bb236d0b1bc95a1202e392ca84ce0644bf628398fbb9517287da8",
            "b704a73caebca38623bb5d22",
        ),
        (
            "<rss><channel><item><guid>first\rsecond</guid><link>https://example.org/post</link></item></channel></rss>",
            "first\nsecond",
            "https://example.org/post",
            "4252f8d56b4bb236d0b1bc95a1202e392ca84ce0644bf628398fbb9517287da8",
            "b704a73caebca38623bb5d22",
        ),
        (
            "<rss><channel><item><guid><![CDATA[first\r\nsecond]]></guid><link>https://example.org/post</link></item></channel></rss>",
            "first\nsecond",
            "https://example.org/post",
            "4252f8d56b4bb236d0b1bc95a1202e392ca84ce0644bf628398fbb9517287da8",
            "b704a73caebca38623bb5d22",
        ),
        (
            "<feed><entry><link href=\"https://example.org/first\r\nsecond\"/></entry></feed>",
            "https://example.org/first\nsecond",
            "https://example.org/first\nsecond",
            "1e60ac49b1f98d495a3d373dd7416efcdc5aaca81b0d387d1b1b8909587a7caa",
            "e66f07ccd2089fba4f00759a",
        ),
        (
            "<rss><channel><item><guid>post <![CDATA[1]]></guid><link>https://example.org/post</link></item></channel></rss>",
            "post1",
            "https://example.org/post",
            "0c99c0ff97ab9d918039af1f390708ea1e0a32feed5c43469328fe2b559c9260",
            "d3de15aef4a7f86bb7aa4c56",
        ),
        (
            "<rss><channel><item><guid>pre <value>ignored</value> post</guid><link>https://example.org/post</link></item></channel></rss>",
            "prepost",
            "https://example.org/post",
            "1accc940716d0d1956412ac8c9ed6049a94e1141e572b908429b7aa3b06212e3",
            "39cbc9f03c64af50ffb28d4d",
        ),
        (
            "<rss><channel><item><guid>pre <!--ignored--> post</guid><link>https://example.org/post</link></item></channel></rss>",
            "pre  post",
            "https://example.org/post",
            "14705c6dd789eb8a78275ba3030fd97943ff9e87e442cd8ab9b4baf001888d6c",
            "fc03158660584246956da9ef",
        ),
        (
            "<rss><channel><item><guid>first</guid><guid>second</guid><id>stable-id</id><link>https://example.org/post</link></item></channel></rss>",
            "stable-id",
            "https://example.org/post",
            "b1def59c1c5d69343801d03c4713526730d1e3bfcc4433c6833ec47a05d94601",
            "3ff7db0ef03790598da90d81",
        ),
        (
            "<rss><channel><item guid=\"attribute-key\"><guid>element-key</guid><link>https://example.org/post</link></item></channel></rss>",
            "attribute-key",
            "https://example.org/post",
            "cb50fad2885cdd9620e8b5d775d90d2538b970087d348d38000670cb673d7287",
            "6532279af5fbf20ae968129a",
        ),
        (
            "<rss><channel><item><link><href>https://example.org/post</href></link></item></channel></rss>",
            "https://example.org/post",
            "https://example.org/post",
            "53db375c90f95b28ded98a853f86ff3611560d726a857caf6b5c71b14789596f",
            "ff28c1b9c4c672e5889547ed",
        ),
        // Scalar ATTRIBUTE identities also see the pre-parse line-ending
        // normalization: a CRLF inside `guid="…"` is LF on both sides.
        (
            "<rss><channel><item guid=\"first\r\nsecond\"><link>https://example.org/post</link></item></channel></rss>",
            "first\nsecond",
            "https://example.org/post",
            "4252f8d56b4bb236d0b1bc95a1202e392ca84ce0644bf628398fbb9517287da8",
            "b704a73caebca38623bb5d22",
        ),
        // Same for a bare CR inside an Atom entry `id="…"` attribute.
        (
            "<feed><entry id=\"first\rsecond\"><link href=\"https://example.org/post\"/></entry></feed>",
            "first\nsecond",
            "https://example.org/post",
            "4252f8d56b4bb236d0b1bc95a1202e392ca84ce0644bf628398fbb9517287da8",
            "b704a73caebca38623bb5d22",
        ),
        // A text run buffered before CDATA keeps its position: the pending
        // segment flushes first, so the key preserves source order.
        (
            "<rss><channel><item><guid>pre<!--x--><![CDATA[ mid ]]><!--y-->post</guid><link>https://example.org/post</link></item></channel></rss>",
            "pre mid post",
            "https://example.org/post",
            "f97474f81145c0d03baecfa3b28863b56b996c265ef868a8eedea533d525a6dc",
            "db2a7886835f457fb9b14f6e",
        ),
    ] {
        let items = parse_xml_feed(xml).unwrap();
        assert_eq!(items.len(), 1, "xml: {xml}");
        assert_eq!(items[0].key, key, "xml: {xml}");
        assert_eq!(items[0].url, url, "xml: {xml}");
        assert_eq!(item_key(&items[0]).unwrap(), hash, "xml: {xml}");
        assert_eq!(
            delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
            nonce,
            "xml: {xml}"
        );
    }
    // Adversarial shapes beyond the corpus, same legacy contract: padded
    // CDATA trims, PIs split runs, empty CDATA joins, entry `link`
    // attributes shadow child links, and any valued link attribute (even
    // `xmlns`) blocks the bare-text fallback.
    for (xml, key, url) in [
        (
            "<rss><channel><item><guid>  <![CDATA[  padded  ]]>  </guid><link>https://example.org/p</link></item></channel></rss>",
            "padded",
            "https://example.org/p",
        ),
        (
            "<rss><channel><item><guid>pre <?pi data?> post</guid><link>https://example.org/p</link></item></channel></rss>",
            "prepost",
            "https://example.org/p",
        ),
        (
            "<rss><channel><item><guid>a<![CDATA[]]>b</guid><link>https://example.org/p</link></item></channel></rss>",
            "ab",
            "https://example.org/p",
        ),
        (
            "<rss><channel><item><link href=\"https://example.org/attr\"><href>https://example.org/child</href></link></item></channel></rss>",
            "https://example.org/attr",
            "https://example.org/attr",
        ),
        (
            "<rss><channel><item link=\"https://example.org/attr\"><link>https://example.org/elem</link><guid>k</guid></item></channel></rss>",
            "k",
            "https://example.org/attr",
        ),
        (
            "<rss><channel><item><link xmlns=\"http://www.w3.org/2005/Atom\">https://example.org/t</link><guid>k</guid></item></channel></rss>",
            "k",
            "https://example.org/t",
        ),
    ] {
        // The xmlns case drops its link (record has no href), so the item
        // falls back to guid-as-key with no valid URL and is filtered out.
        let items = parse_xml_feed(xml).unwrap();
        if url == "https://example.org/t" && key == "k" {
            assert!(items.is_empty(), "xml: {xml}");
        } else {
            assert_eq!(items.len(), 1, "xml: {xml}");
            assert_eq!(items[0].key, key, "xml: {xml}");
            assert_eq!(items[0].url, url, "xml: {xml}");
        }
    }
    // Multi-href children are an array legacy rejects, so the item is lost
    // deterministically on both sides (no URL, no key fallback).
    assert!(
        parse_xml_feed("<rss><channel><item><link><href>https://example.org/1</href><href>https://example.org/2</href></link></item></channel></rss>")
            .unwrap()
            .is_empty()
    );
    // A nested element child projects the link to a record with no `href`
    // field, so legacy discards the item instead of URL-keying it.
    assert!(
        parse_xml_feed("<rss><channel><item><link>https://example.org/post<other>value</other></link></item></channel></rss>")
            .unwrap()
            .is_empty()
    );
    // Alternate selection reads child `<rel>` through the same
    // attribute-over-child projection: the alternate post URL wins over the
    // self feed URL even with a stable id in play.
    let items = parse_xml_feed("<feed><entry><id>stable-id</id><link><rel>self</rel><href>https://example.org/feed</href></link><link><rel>alternate</rel><href>https://example.org/post</href></link></entry></feed>").unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "stable-id");
    assert_eq!(items[0].url, "https://example.org/post");
    assert_eq!(
        item_key(&items[0]).unwrap(),
        "b1def59c1c5d69343801d03c4713526730d1e3bfcc4433c6833ec47a05d94601"
    );
    assert_eq!(
        delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
        "3ff7db0ef03790598da90d81"
    );
    // A same-named attribute shadows a `<rel>` child the way attributes
    // overwrite same-named children on the legacy record: `rel="self"` on the
    // link wins over `<rel>alternate</rel>`, so the suitable alternate is the
    // OTHER link, not this one.
    let items = parse_xml_feed("<feed><entry><id>k</id><link rel=\"self\"><rel>alternate</rel><href>https://example.org/u1</href></link><link rel=\"alternate\"><rel>self</rel><href>https://example.org/u2</href></link></entry></feed>").unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url, "https://example.org/u2");
    // A PI child is a tagged record field (`?pi`), so a text link with a PI
    // is a record without `href` and the item is discarded — while a comment
    // tail or a CDATA body is `#text` and the bare-text fallback survives.
    assert!(parse_xml_feed(
        "<rss><channel><item><link>https://example.org/p<?pi data?></link></item></channel></rss>"
    )
    .unwrap()
    .is_empty());
    assert!(
        parse_xml_feed("<rss><channel><item><link rel=\"alternate\">https://example.org/p</link></item></channel></rss>")
            .unwrap()
            .is_empty()
    );
    for (xml, key, url, hash, nonce) in [
        (
            "<rss><channel><item><link>https://example.org/p<!--c--></link></item></channel></rss>",
            "https://example.org/p",
            "https://example.org/p",
            "a88aada49b36ba941d5df671fa8d3a3f244d9606589ce8e0ed6a1fc1bdf4509c",
            "b191ee155dacf0e38c4b002a",
        ),
        (
            "<rss><channel><item><link><![CDATA[https://example.org/p]]></link></item></channel></rss>",
            "https://example.org/p",
            "https://example.org/p",
            "a88aada49b36ba941d5df671fa8d3a3f244d9606589ce8e0ed6a1fc1bdf4509c",
            "b191ee155dacf0e38c4b002a",
        ),
        // CDATA flushes the text run buffered before it; empty CDATA still
        // joins neighbours; CDATA chains across an element keep order.
        (
            "<rss><channel><item><guid>pre <!--c--><![CDATA[1]]></guid><link>https://example.org/p</link></item></channel></rss>",
            "pre1",
            "https://example.org/p",
            "d884b328056ffc0a71e6e6cc0d30a336c252732d7c6a9381330a4ba462418c75",
            "0d8c240da97f3e6c4391a281",
        ),
        (
            "<rss><channel><item><guid>a <![CDATA[]]> b</guid><link>https://example.org/p</link></item></channel></rss>",
            "ab",
            "https://example.org/p",
            "fb8e20fc2e4c3f248c60c39bd652f3c1347298bb977b8b4d5903b85055620603",
            "04335f7f7bfcc511c74fc79c",
        ),
        (
            "<rss><channel><item><guid>a<![CDATA[x]]><v>q</v><![CDATA[y]]>b</guid><link>https://example.org/p</link></item></channel></rss>",
            "axyb",
            "https://example.org/p",
            "8ff031f9e83eb5c3635706b88d9529b7269bd2038a361c870ec95ea12f42d3b5",
            "45798480c1c9154060b5505f",
        ),
        (
            "<rss><channel><item><guid><![CDATA[1]]> post</guid><link>https://example.org/p</link></item></channel></rss>",
            "1post",
            "https://example.org/p",
            "e0e05d90c9af2475b7190e3464809b5e2ed43d2f1f2bb3aa800083c2e1849f73",
            "e52c9770ae9a031d4753abf3",
        ),
    ] {
        let items = parse_xml_feed(xml).unwrap();
        assert_eq!(items.len(), 1, "xml: {xml}");
        assert_eq!(items[0].key, key, "xml: {xml}");
        assert_eq!(items[0].url, url, "xml: {xml}");
        assert_eq!(item_key(&items[0]).unwrap(), hash, "xml: {xml}");
        assert_eq!(
            delivery_nonce("feed-1", &item_key(&items[0]).unwrap()),
            nonce,
            "xml: {xml}"
        );
    }
}

#[test]
fn xml_resource_bombs_are_rejected_in_subprocess() {
    if std::env::var_os("TWO_FEED_XML_RESOURCE_PROBE").is_some() {
        let deep = format!(
            "<rss><channel><item>{}<guid>one</guid>{}</item></channel></rss>",
            "<x>".repeat(50_000),
            "</x>".repeat(50_000)
        );
        let attributes: String = (0..170_000).map(|i| format!(" a{i}=\"v\"")).collect();
        let wide = format!("<rss><channel><item{attributes}/></channel></rss>");
        for xml in [deep, wide] {
            assert!(xml.len() < MAX_FEED_BYTES);
            assert!(matches!(parse_xml_feed(&xml), Err(FeedError::InvalidXml)));
        }
        return;
    }
    // A stack overflow aborts, rather than unwinds. Keep the adversarial
    // fixtures in a child using the ordinary test-thread stack and bound
    // its lifetime so a quadratic-parser regression cannot hang the suite.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "xml_resource_bombs_are_rejected_in_subprocess"])
        .env("TWO_FEED_XML_RESOURCE_PROBE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("XML resource rejection exceeded the subprocess deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "XML probe failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn xml_depth_and_attribute_limits_are_independent_of_body_and_nodes() {
    let depth_xml = |depth: usize| {
        format!(
            "<rss>{}{}</rss>",
            "<x>".repeat(depth - 1),
            "</x>".repeat(depth - 1)
        )
    };
    assert!(parse_xml_feed(&depth_xml(64)).is_ok());
    assert!(matches!(
        parse_xml_feed(&depth_xml(65)),
        Err(FeedError::InvalidXml)
    ));
    // Self-closing elements also occupy a depth level.
    let xml = format!("<rss>{}<x/>{}</rss>", "<x>".repeat(63), "</x>".repeat(63));
    assert!(matches!(parse_xml_feed(&xml), Err(FeedError::InvalidXml)));
    let attributes =
        |count: usize| -> String { (0..count).map(|i| format!(" a{i}=\"v\"")).collect() };
    assert!(parse_xml_feed(&format!("<rss{}/>", attributes(64))).is_ok());
    assert!(matches!(
        parse_xml_feed(&format!("<rss{}/>", attributes(65))),
        Err(FeedError::InvalidXml)
    ));
    // Namespace declarations count too, including those that roxmltree
    // drops from the ordinary attribute collection.
    let namespaces: String = (0..65)
        .map(|i| format!(" xmlns:n{i}=\"urn:n{i}\""))
        .collect();
    assert!(matches!(
        parse_xml_feed(&format!("<rss{namespaces}/>")),
        Err(FeedError::InvalidXml)
    ));
    let tag = format!("<x{}/>", attributes(64));
    assert!(parse_xml_feed(&format!("<rss>{}</rss>", tag.repeat(64))).is_ok());
    assert!(matches!(
        parse_xml_feed(&format!("<rss>{}</rss>", tag.repeat(65))),
        Err(FeedError::InvalidXml)
    ));
    // Markup in opaque spans and quoted values is not real nesting or
    // attributes. A quote-aware, delimiter-aware scan must not false-reject.
    let xml = format!("<?pi {}?><rss note='{}'><channel><item><guid><![CDATA[{}]]><!--{}--></guid><link>https://example.org/p</link></item></channel></rss>", "<x a=\"v\">".repeat(100), " > a=\"v\"".repeat(100), "<x a=\"v\">".repeat(100), "<x a=\"v\">".repeat(100));
    assert_eq!(parse_xml_feed(&xml).unwrap().len(), 1);
    for xml in [
        "<rss><!--",
        "<rss><![CDATA[",
        "<rss><?pi",
        "<rss a=\"unterminated>",
        "</rss><rss/>",
    ] {
        assert!(matches!(parse_xml_feed(xml), Err(FeedError::InvalidXml)));
    }
}

#[test]
fn feed_container_projection_matches_legacy_records() {
    let item = "<item><guid>one</guid><link>https://example.org/p</link></item>";
    for xml in [
        format!("<rss channel=\"ignored\"><channel>{item}</channel></rss>"),
        format!("<rss><channel item=\"ignored\">{item}</channel></rss>"),
        format!("<rss><channel>{item}</channel><channel>{item}</channel></rss>"),
        "<feed entry=\"ignored\"><entry><id>one</id><link href=\"https://example.org/p\"/></entry></feed>".into(),
        format!("<rss channel=\"\"><channel>{item}</channel></rss>"),
        format!("<rss><channel item=\"\">{item}</channel></rss>"),
        "<feed entry=\"\"><entry><id>one</id><link href=\"https://example.org/p\"/></entry></feed>".into(),
    ] {
        assert!(parse_xml_feed(&xml).unwrap().is_empty(), "xml: {xml}");
    }
    let xml = format!("<rss><channel>{item}{item}</channel></rss>");
    assert_eq!(parse_xml_feed(&xml).unwrap().len(), 2);
    let xml = "<feed><entry><id>one</id><link href=\"https://example.org/p\"/></entry><entry><id>two</id><link href=\"https://example.org/q\"/></entry></feed>";
    assert_eq!(parse_xml_feed(xml).unwrap().len(), 2);
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
    let xml = format!(
        "<rss><channel><item><guid>long</guid><link>https://example.org/{}</link></item></channel></rss>",
        "a".repeat(2000)
    );
    assert!(parse_xml_feed(&xml).unwrap().is_empty());
}

#[test]
fn item_url_budget_matches_each_feed_kind() {
    const URL_PREFIX: &str = "https://example.org/";
    let url_with_units =
        |units: usize| format!("{URL_PREFIX}{}", "a".repeat(units - URL_PREFIX.len()));

    for (kind, prefix) in [
        (FeedKind::Rss, "New feed item"),
        (FeedKind::Youtube, "New YouTube upload"),
        (FeedKind::Twitch, "Twitch update"),
    ] {
        let max_url_units = 2000 - format!("{prefix}: ****\n").encode_utf16().count() - 1;
        let xml_for_url = |url: &str| {
            match kind {
            FeedKind::Youtube => format!(
                "<feed><entry><id>key</id><title>x</title><link href=\"{url}\"/></entry></feed>"
            ),
            FeedKind::Rss | FeedKind::Twitch => format!(
                "<rss><channel><item><guid>key</guid><title>x</title><link>{url}</link></item></channel></rss>"
            ),
        }
        };

        let accepted_url = url_with_units(max_url_units);
        let parsed = parse_xml_feed_for_kind(&xml_for_url(&accepted_url), kind).unwrap();
        assert_eq!(parsed.len(), 1, "kind: {kind:?}");
        let post = plan_post(&relay(kind), &parsed[0]).unwrap();
        assert_eq!(post.content.encode_utf16().count(), 2000, "kind: {kind:?}");

        let over_budget_url = url_with_units(max_url_units + 1);
        assert!(
            parse_xml_feed_for_kind(&xml_for_url(&over_budget_url), kind)
                .unwrap()
                .is_empty()
        );
        let kind_agnostic = parse_xml_feed(&xml_for_url(&over_budget_url)).unwrap();
        if kind == FeedKind::Youtube {
            assert_eq!(
                kind_agnostic.len(),
                1,
                "kind-agnostic parser uses the widest feed URL budget"
            );
        } else {
            assert!(kind_agnostic.is_empty(), "kind-agnostic parser: {kind:?}");
        }
        let direct_item = FeedItem {
            title: "x".into(),
            url: over_budget_url,
            ..item("over-budget")
        };
        assert!(matches!(
            plan_post(&relay(kind), &direct_item),
            Err(FeedError::InvalidItemUrl)
        ));
    }
}

#[test]
fn message_safe_item_urls_preserve_unrelated_at_signs() {
    let feed = relay(FeedKind::Rss);
    let feed_item = FeedItem {
        title: "Post".into(),
        url: "https://example.org/@alice/post?next=%40everyone&also=%40here".into(),
        ..item("mention-url")
    };
    let post = plan_post(&feed, &feed_item).unwrap();
    let item_url = "https://example.org/@alice/post?next=%40everyone&also=%40here";

    assert!(post.content.ends_with(item_url));
    assert_eq!(
        two_bot_core::message_safety::content(&post.content),
        post.content
    );
}

#[test]
fn feed_posts_escape_untrusted_titles_and_preserve_item_url_previews() {
    let feed = relay(FeedKind::Rss);
    let feed_item = FeedItem {
        title: "x** [Claim](https://evil.example) **y\r\n@everyone".into(),
        url: "https://example.org/post?x=1&y=2".into(),
        ..item("rss-1")
    };
    let content = plan_post(&feed, &feed_item).unwrap().content;

    assert!(!content.contains("]("));
    assert!(content.contains(r"\[Claim\]\(https\:\/\/evil\.example\)"));
    assert_eq!(content.lines().count(), 2);
    assert!(content.ends_with("https://example.org/post?x=1&y=2"));
    assert_eq!(two_bot_core::message_safety::content(&content), content);

    let long_title = FeedItem {
        title: "*".repeat(1200),
        url: "https://example.org/post?x=1&y=2".into(),
        ..item("rss-long")
    };
    let long_content = plan_post(&feed, &long_title).unwrap().content;
    assert!(long_content.encode_utf16().count() <= 2000);
    assert!(long_content.contains("…**\nhttps://example.org/post?x=1&y=2"));
    assert!(long_content.ends_with("https://example.org/post?x=1&y=2"));
    assert_eq!(
        two_bot_core::message_safety::content(&long_content),
        long_content
    );

    let url = format!(
        "https://example.org/{}",
        "a".repeat(1970 - "https://example.org/".len())
    );
    let near_limit_mention = FeedItem {
        title: "@everyone".into(),
        url: url.clone(),
        ..item("rss-mention-boundary")
    };
    let near_limit_post = plan_post(&feed, &near_limit_mention).unwrap();
    assert_eq!(near_limit_post.content.encode_utf16().count(), 2000);
    assert!(near_limit_post.content.ends_with(&url));
    assert!(near_limit_post.content.contains("…**\n"));
    assert_eq!(
        two_bot_core::message_safety::content(&near_limit_post.content),
        near_limit_post.content
    );

    let long_url = FeedItem {
        url: format!("https://example.org/{}", "a".repeat(2000)),
        ..item("rss-long-url")
    };
    assert!(matches!(
        plan_post(&feed, &long_url),
        Err(FeedError::InvalidItemUrl)
    ));

    assert!(matches!(
        plan_post(
            &feed,
            &FeedItem {
                url: "https://user@example.org/post".into(),
                ..feed_item
            }
        ),
        Err(FeedError::InvalidItemUrl)
    ));
}

#[test]
fn unsafe_item_urls_are_filtered_before_delivery() {
    let feed = relay(FeedKind::Rss);
    for url in [
        "https://example.org/@everyone",
        "https://example.org/notice?search=@here",
        "https://example.org/x](https://evil.example)",
    ] {
        let xml = format!(
            "<rss><channel><item><guid>key</guid><title>x</title><link>{url}</link></item></channel></rss>"
        );
        assert!(parse_xml_feed_for_kind(&xml, FeedKind::Rss)
            .unwrap()
            .is_empty());
        assert!(matches!(
            plan_post(
                &feed,
                &FeedItem {
                    url: url.into(),
                    ..item("unsafe-url")
                }
            ),
            Err(FeedError::InvalidItemUrl)
        ));
    }

    for url in [
        "https://example.org/%40everyone",
        "https://en.wikipedia.org/wiki/Foo_(bar)",
    ] {
        let xml = format!(
            "<rss><channel><item><guid>key</guid><title>x</title><link>{url}</link></item></channel></rss>"
        );
        let items = parse_xml_feed_for_kind(&xml, FeedKind::Rss).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, url);

        let post = plan_post(
            &feed,
            &FeedItem {
                url: url.into(),
                ..item("kept-url")
            },
        )
        .unwrap();
        assert!(post.content.ends_with(url));
        assert_eq!(
            two_bot_core::message_safety::content(&post.content),
            post.content
        );
    }
}

#[test]
fn parse_report_counts_only_items_filtered_for_unsafe_urls() {
    let entry = |key: &str, url: &str| {
        format!("<item><guid>{key}</guid><title>x</title><link>{url}</link></item>")
    };
    // Literal mentions, handles that merely start with `here`/`everyone` and
    // `](` masked links are filtered and counted; a plain handle, a path with
    // parentheses, an invalid URL and a credentialed URL are kept or dropped
    // without touching the count.
    let xml = format!(
        "<rss><channel>{}{}{}{}{}{}{}</channel></rss>",
        entry("a", "https://example.org/@everyone"),
        entry("b", "https://mastodon.social/@heresy/1"),
        entry("c", "https://mastodon.social/@alice/1"),
        entry("d", "not a url"),
        entry("e", "https://user@example.org/@here"),
        entry("f", "https://example.org/x](https://evil.example)"),
        entry("g", "https://en.wikipedia.org/wiki/Foo_(bar)"),
    );
    let parsed = parse_xml_feed_report(&xml, FeedKind::Rss).unwrap();
    assert_eq!(parsed.unsafe_urls_filtered, 3);
    assert_eq!(parsed.items.len(), 2);
    assert_eq!(parsed.items[0].url, "https://mastodon.social/@alice/1");
    assert_eq!(
        parsed.items[1].url,
        "https://en.wikipedia.org/wiki/Foo_(bar)"
    );
    assert_eq!(
        parse_xml_feed_for_kind(&xml, FeedKind::Rss).unwrap(),
        parsed.items
    );
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

#[test]
fn feed_list_truncates_each_source_before_combining_rows() {
    let first = FeedRelay {
        source: format!("https://example.org/{}", "a".repeat(5000)),
        ..relay(FeedKind::Rss)
    };
    let second = FeedRelay {
        id: "second".into(),
        source: "https://example.org/second".into(),
        ..relay(FeedKind::Rss)
    };
    let text = feed_list_text(&[first, second]);

    assert!(text.contains("…\n`second`"));
    assert!(text.encode_utf16().count() <= 2000);
}

#[test]
fn feed_add_rejects_long_sources_and_nonstandard_ports() {
    let context = FeedCommandContext {
        enabled: true,
        configured_guild_id: "guild-1",
        guild_id: "guild-1",
        channel_id: "channel-1",
        actor_id: "actor",
        can_manage_guild: true,
        now_ms: 1234,
    };
    let expanded_source = format!("https://example.org/{}", "é".repeat(400));
    assert!(expanded_source.len() < MAX_FEED_SOURCE_BYTES);
    let sources = [
        format!("https://example.org/{}", "a".repeat(MAX_FEED_SOURCE_BYTES)),
        "https://example.org:8443/feed".into(),
        expanded_source,
    ];

    for source in sources {
        assert!(
            matches!(
                plan_command(
                    &context,
                    FeedCommand::Add {
                        id: "feed-1".into(),
                        kind: FeedKind::Rss,
                        source: source.clone(),
                    }
                ),
                Err(FeedError::Fetch(FetchError::InvalidSource))
            ),
            "accepted {source}"
        );
    }
}
