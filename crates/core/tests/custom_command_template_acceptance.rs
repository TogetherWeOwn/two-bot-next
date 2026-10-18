//! Acceptance tests: custom-command template validation and rendering hold
//! under hostile input (pure, offline, no Discord call).
//!
//! Acceptance contract for `two_bot_core::custom_commands` template entry
//! points (`placeholders_in`, `validate_template`, `render_template`):
//!
//! - Oversized templates fail as bounded `TemplateError::InvalidLength`,
//!   measured in UTF-16 units (legacy `string.length`); length refusal comes
//!   first, before any placeholder inspection.
//! - Degenerate brace shapes (`{}`, `{User}`, `{user-name}`, or unclosed
//!   braces) are literal text: they validate clean and render byte-identical,
//!   never panicking on any byte path. A doubled opening brace before a known
//!   placeholder leaves the first brace literal and substitutes the inner
//!   placeholder once (for example, `{{user}}` renders as `{<user>}`).
//! - Placeholder extraction is total-bounded: at most `MAX_PLACEHOLDERS`
//!   distinct keys in first-seen order, deterministic; validation refuses
//!   unknown keys with the exact offending key and never reports
//!   `RenderTooLong`.
//! - Rendering never panics: hostile expansion refuses as bounded
//!   `RenderTooLong` past Discord's ceiling, context text renders verbatim
//!   in a single pass (no re-expansion), and both paths are deterministic.
//!
//! Test files only; no runtime code is touched here. These pins exercise the
//! public API from outside the crate, complementing the in-crate hostile
//! regressions and the `custom_command_template` fuzz target (seed corpus
//! replayed in CI `fuzz-compile`).

use two_bot_core::custom_commands::{
    placeholders_in, render_template, validate_template, TemplateContext, TemplateError,
    MAX_PLACEHOLDERS, MAX_RENDERED_CHARS, MAX_TEMPLATE_CHARS,
};

fn ctx() -> TemplateContext {
    TemplateContext {
        user: "<@1>".to_owned(),
        username: "one".to_owned(),
        server: "TWO".to_owned(),
        channel: "#general".to_owned(),
    }
}

fn alpha_key(mut n: usize) -> String {
    let mut key = Vec::new();
    loop {
        key.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    key.into_iter().rev().collect()
}

#[test]
fn oversized_templates_refuse_as_bounded_invalid_length() {
    assert_eq!(validate_template(""), Err(TemplateError::InvalidLength));
    assert_eq!(
        validate_template(&"x".repeat(MAX_TEMPLATE_CHARS + 1)),
        Err(TemplateError::InvalidLength)
    );
    validate_template(&"x".repeat(MAX_TEMPLATE_CHARS)).expect("ceiling ok");
}

#[test]
fn length_counts_utf16_units_including_emoji() {
    // 😀 is two UTF-16 units: 1000 fit exactly, 1001 refuse.
    validate_template(&"😀".repeat(1000)).expect("2000 UTF-16 units");
    assert_eq!(
        validate_template(&"😀".repeat(1001)),
        Err(TemplateError::InvalidLength)
    );
}

#[test]
fn length_refusal_comes_before_placeholder_inspection() {
    // Oversized AND carrying an unknown key: the bounded length error wins,
    // so the error echo never grows with the template.
    let template = format!("{{usre}}{}", "x".repeat(MAX_TEMPLATE_CHARS + 1));
    assert_eq!(
        validate_template(&template),
        Err(TemplateError::InvalidLength)
    );
}

#[test]
fn degenerate_braces_are_literal_not_placeholders() {
    for literal in [
        "{User}",
        "{USER}",
        "{}",
        "{user-name}",
        "{user name}",
        "{user!",
        "{user",
        "user}",
        "{",
        "}",
        "{{{}}}",
    ] {
        validate_template(literal).expect("degenerate braces are literal");
        assert!(
            placeholders_in(literal).is_empty(),
            "{literal:?} extracts nothing"
        );
    }
}

#[test]
fn degenerate_braces_render_byte_identical() {
    let ctx = ctx();
    for literal in [
        "{User} {USER} {} {user-name}",
        "{user",
        "user}",
        "{{}}",
        "{ }",
    ] {
        assert_eq!(
            render_template(literal, &ctx),
            Ok(literal.to_owned()),
            "{literal:?} renders literally"
        );
    }
}

#[test]
fn doubled_braces_wrap_one_substitution() {
    // `{{user}}`: the first brace is literal, the inner pair substitutes once.
    let mut ctx = ctx();
    ctx.user = "U".to_owned();
    assert_eq!(render_template("{{user}}", &ctx), Ok("{U}".to_owned()));
}

#[test]
fn placeholder_extraction_dedupes_in_first_seen_order() {
    assert_eq!(
        placeholders_in("{user} and {user} and {server}"),
        ["user".to_owned(), "server".to_owned()]
    );
}

#[test]
fn placeholder_extraction_is_total_bounded_on_hostile_input() {
    let hostile: String = (0..200).map(|i| format!("{{{}}}", alpha_key(i))).collect();
    let found = placeholders_in(&hostile);
    assert_eq!(found.len(), MAX_PLACEHOLDERS);
    assert_eq!(found[0], alpha_key(0));
    assert_eq!(found[1], alpha_key(1));
    // Deterministic across calls.
    assert_eq!(found, placeholders_in(&hostile));
}

#[test]
fn unknown_placeholder_refuses_with_the_exact_key() {
    assert_eq!(
        validate_template("Hi {user}, see {usre}"),
        Err(TemplateError::UnknownPlaceholder("usre".to_owned()))
    );
}

#[test]
fn validate_never_reports_render_too_long() {
    // Validation is a definition-time check: even a template that would
    // render past the ceiling fails here only with length/unknown errors.
    let hostile: String = (0..200).map(|i| format!("{{{}}}", alpha_key(i))).collect();
    match validate_template(&hostile) {
        Err(TemplateError::RenderTooLong(_)) => {
            panic!("validate never renders, so it never reports RenderTooLong")
        }
        Err(TemplateError::InvalidLength) | Err(TemplateError::UnknownPlaceholder(_)) => {}
        Ok(()) => panic!("200 distinct keys must contain an unknown placeholder"),
    }
    // A render-huge but definition-valid template validates clean.
    validate_template(&"{username}".repeat(200)).expect("known keys validate");
}

#[test]
fn render_substitutes_all_four_placeholders() {
    assert_eq!(
        render_template("hey {user} ({username}) of {server} in {channel}", &ctx()),
        Ok("hey <@1> (one) of TWO in #general".to_owned())
    );
}

#[test]
fn hostile_expansion_refuses_as_bounded_render_too_long() {
    let big = TemplateContext {
        user: "y".repeat(10_000),
        username: String::new(),
        server: String::new(),
        channel: String::new(),
    };
    match render_template(&"{user}".repeat(500), &big) {
        Err(TemplateError::RenderTooLong(len)) => {
            assert_eq!(
                len,
                big.user.len(),
                "byte tripwire must refuse before a second hostile expansion, got {len}"
            );
        }
        other => panic!("expected bounded RenderTooLong, got {other:?}"),
    }
}

#[test]
fn over_ceiling_literal_refuses_with_exact_length() {
    let empty = TemplateContext {
        user: String::new(),
        username: String::new(),
        server: String::new(),
        channel: String::new(),
    };
    assert_eq!(
        render_template(&"x".repeat(MAX_RENDERED_CHARS + 1), &empty),
        Err(TemplateError::RenderTooLong(MAX_RENDERED_CHARS + 1))
    );
}

#[test]
fn render_is_single_pass_and_preserves_unicode() {
    // Context text renders verbatim, never re-scanned: a context value that
    // looks like a placeholder stays literal in the output.
    let ctx = TemplateContext {
        user: "{username}".to_owned(),
        username: "not substituted twice".to_owned(),
        server: "世界".to_owned(),
        channel: "#rules".to_owned(),
    };
    let template = "😀 {user} {{server}} {User} {unknown}";
    let out = render_template(template, &ctx).expect("renders");
    assert_eq!(out, "😀 {username} {世界} {User} {unknown}");
    assert_eq!(render_template(template, &ctx), Ok(out));
}
