#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::custom_commands::{
    placeholders_in, render_template, validate_template, TemplateContext, TemplateError,
    MAX_PLACEHOLDERS, MAX_RENDERED_CHARS, MAX_TEMPLATE_CHARS, TEMPLATE_PLACEHOLDERS,
};

/// Bounds keep every allocation a function of the (libFuzzer-capped) input:
/// the template may reach past the 2000-unit stored bound so oversized paths
/// stay reachable, while each context field stays small enough that even a
/// fully-expanded render stays computable (the production renderer refuses
/// once buffered bytes breach a byte tripwire, materialising at most one
/// expansion past it).
const MAX_TEMPLATE_FIELD_CHARS: usize = 4096;
const MAX_CTX_FIELD_CHARS: usize = 512;
/// At most template + four context fields; extras are ignored.
const MAX_FIELDS: usize = 5;

const TEMPLATE_IDX: usize = 0;
const USER_IDX: usize = 1;
const USERNAME_IDX: usize = 2;
const SERVER_IDX: usize = 3;
const CHANNEL_IDX: usize = 4;

fn truncate_chars(s: &str, limit: usize) -> String {
    s.chars().take(limit).collect()
}

fn is_placeholder_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase())
}

fuzz_target!(|data: &[u8]| {
    // Input-independent base cases: known-good templates still validate and
    // render, oversized/unknown still refuse as bounded errors, and the
    // hostile extraction/render pins hold without the fuzzer. Any behavior
    // change must update this target on its own card; silent drift fails the
    // fuzzer instead.
    validate_template("Hello {user} in {server}/{channel} aka {username}").expect("valid");
    assert_eq!(validate_template(""), Err(TemplateError::InvalidLength));
    assert_eq!(
        validate_template(&"x".repeat(MAX_TEMPLATE_CHARS + 1)),
        Err(TemplateError::InvalidLength)
    );
    validate_template(&"x".repeat(MAX_TEMPLATE_CHARS)).expect("ceiling ok");
    assert_eq!(
        validate_template("Hi {usre}"),
        Err(TemplateError::UnknownPlaceholder("usre".to_owned()))
    );
    // Legacy literal shapes never become placeholders.
    validate_template("Hi {User} {USER} {} {user-name}").expect("all literal");
    assert_eq!(placeholders_in("{User} {}"), Vec::<String>::new());
    assert_eq!(
        placeholders_in("{user} and {user} and {server}"),
        ["user".to_owned(), "server".to_owned()]
    );
    // Total-bounded extraction pin: 200 distinct keys collapse to the cap.
    let hostile: String = (0..200)
        .map(|i| {
            let mut n = i;
            let mut key = Vec::new();
            loop {
                key.push((b'a' + (n % 26) as u8) as char);
                n /= 26;
                if n == 0 {
                    break;
                }
                n -= 1;
            }
            format!("{{{}}}", key.into_iter().rev().collect::<String>())
        })
        .collect();
    let capped = placeholders_in(&hostile);
    assert!(capped.len() <= MAX_PLACEHOLDERS);
    assert_eq!(capped.len(), MAX_PLACEHOLDERS);
    // Hostile expansion pin: the byte tripwire refuses once buffered bytes
    // breach it (here the second push sees 10_000 buffered bytes), reporting
    // bytes always past the ceiling instead of the full 5M expansion.
    let big_ctx = TemplateContext {
        user: "y".repeat(10_000),
        username: String::new(),
        server: String::new(),
        channel: String::new(),
    };
    assert!(
        matches!(
            render_template(&"{user}".repeat(500), &big_ctx),
            Err(TemplateError::RenderTooLong(len)) if len > MAX_RENDERED_CHARS
        ),
        "hostile expansion must refuse as bounded RenderTooLong"
    );

    // Member-supplied templates arrive as UTF-8 text. Invalid UTF-8 is
    // discarded, matching the other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let mut fields: Vec<String> = raw
        .split('\0')
        .enumerate()
        .map(|(idx, part)| {
            let limit = if idx == TEMPLATE_IDX {
                MAX_TEMPLATE_FIELD_CHARS
            } else {
                MAX_CTX_FIELD_CHARS
            };
            truncate_chars(part, limit)
        })
        .collect();
    if fields.len() > MAX_FIELDS {
        fields.truncate(MAX_FIELDS);
    }
    let field = |idx: usize| fields.get(idx).map(String::as_str).unwrap_or("");
    let template = truncate_chars(field(TEMPLATE_IDX), MAX_TEMPLATE_FIELD_CHARS);
    let ctx = TemplateContext {
        user: truncate_chars(field(USER_IDX), MAX_CTX_FIELD_CHARS),
        username: truncate_chars(field(USERNAME_IDX), MAX_CTX_FIELD_CHARS),
        server: truncate_chars(field(SERVER_IDX), MAX_CTX_FIELD_CHARS),
        channel: truncate_chars(field(CHANNEL_IDX), MAX_CTX_FIELD_CHARS),
    };

    // Placeholder extraction is total-bounded on every input: at most the cap,
    // each a lowercase-alpha run, deduplicated in first-seen order, and
    // deterministic.
    let found = placeholders_in(&template);
    assert!(
        found.len() <= MAX_PLACEHOLDERS,
        "extraction must stay total-bounded"
    );
    for key in &found {
        assert!(is_placeholder_key(key), "extracted key is a lowercase run");
    }
    let mut deduped = found.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), found.len(), "extraction deduplicates keys");
    assert_eq!(
        found,
        placeholders_in(&template),
        "extraction is deterministic"
    );

    // Validation fails closed on every input: oversized length refuses first
    // with a unit-sized error, unknown placeholders refuse with the exact
    // offending key, and success means every extracted key is known.
    // All three error variants stay bounded (unit, key within the template,
    // count within the ceiling).
    let template_units = template.encode_utf16().count();
    match validate_template(&template) {
        Err(TemplateError::InvalidLength) => assert!(
            !(1..=MAX_TEMPLATE_CHARS).contains(&template_units),
            "InvalidLength only outside 1..=2000 UTF-16 units"
        ),
        Err(TemplateError::UnknownPlaceholder(key)) => {
            assert!((1..=MAX_TEMPLATE_CHARS).contains(&template_units));
            assert!(
                !TEMPLATE_PLACEHOLDERS.contains(&key.as_str()),
                "unknown key is not in the supported set"
            );
            assert!(found.contains(&key), "unknown key comes from extraction");
            assert!(
                key.len() <= template.len(),
                "error echo stays within the template"
            );
        }
        Err(TemplateError::RenderTooLong(_)) => {
            panic!("validate never renders, so it never reports RenderTooLong")
        }
        Ok(()) => {
            assert!((1..=MAX_TEMPLATE_CHARS).contains(&template_units));
            assert!(
                found
                    .iter()
                    .all(|p| TEMPLATE_PLACEHOLDERS.contains(&p.as_str())),
                "validated templates reference only known placeholders"
            );
        }
    }
    assert_eq!(
        validate_template(&template).is_ok(),
        validate_template(&template).is_ok(),
        "validation is deterministic"
    );

    // Rendering never panics on any input: success stays within Discord's
    // ceiling, refusal carries the detected over-ceiling length, single-pass
    // substitution never re-expands context text, and both paths are
    // deterministic. No database, Discord client, network or secret is touched
    // here; only these pure string functions run.
    match render_template(&template, &ctx) {
        Ok(out) => {
            assert!(
                out.encode_utf16().count() <= MAX_RENDERED_CHARS,
                "rendered output stays within the ceiling"
            );
            // Context text renders verbatim, never re-scanned: a context value
            // that looks like a placeholder stays literal in the output.
            assert_eq!(
                render_template(&template, &ctx),
                Ok(out.clone()),
                "render is deterministic"
            );
        }
        Err(TemplateError::RenderTooLong(len)) => {
            assert!(
                len > MAX_RENDERED_CHARS,
                "RenderTooLong only past the ceiling"
            );
        }
        Err(other) => panic!("render only refuses over-ceiling output, got {other:?}"),
    }
});
