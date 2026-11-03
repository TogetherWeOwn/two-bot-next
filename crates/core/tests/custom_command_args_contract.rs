//! Slice-1 contract for the bounded `{args}` substitution
//! (`docs/custom-command-args-spec.md` §8, render-level cases).
//!
//! Offline deterministic fixtures only: synthetic context values, no member
//! data, no Discord send, no database. Slash/prefix capture, corrections and
//! audit land with the run-path slice; what this target pins is the core
//! promise — `{args}` renders member text literally in one pass, bounded,
//! through the same mention treatment as every other template input.

use two_bot_core::custom_commands::{
    render_template, validate_args, validate_template, ArgsError, TemplateContext, TemplateError,
    MAX_ARGS_UNITS, MAX_RENDERED_CHARS,
};
use two_bot_core::message_safety::neutralize_mentions;

fn ctx_with_args(args: &str) -> TemplateContext {
    TemplateContext {
        user: "<@3333>".to_owned(),
        username: "tester".to_owned(),
        server: "Test guild".to_owned(),
        channel: "<#4444>".to_owned(),
        args: args.to_owned(),
    }
}

#[test]
fn args_template_validates_and_absent_args_render_empty() {
    // ARG-01 / ARG-02: absent and empty-string values substitute empty, and
    // templates without `{args}` behave byte-identically to before.
    validate_template("Says {args}").expect("{args} is the fifth key");
    assert_eq!(
        render_template("Says {args}", &ctx_with_args("")),
        Ok("Says ".to_owned())
    );
    assert_eq!(
        render_template("Hi {user}", &ctx_with_args("ignored")),
        Ok("Hi <@3333>".to_owned())
    );
}

#[test]
fn args_bound_accepts_500_units_and_refuses_501() {
    // ARG-04 / ARG-05: the 500-unit boundary is authoritative UTF-16 counting.
    assert_eq!(MAX_ARGS_UNITS, 500);
    validate_args(&"x".repeat(500)).expect("boundary renders");
    assert_eq!(validate_args(&"x".repeat(501)), Err(ArgsError::TooLong));
    assert_eq!(
        render_template("Says {args}", &ctx_with_args(&"x".repeat(500))).expect("renders"),
        format!("Says {}", "x".repeat(500))
    );
}

#[test]
fn args_bound_counts_utf16_units_not_chars() {
    // ARG-06: astral characters cost two units each.
    validate_args(&"😀".repeat(250)).expect("250 emoji are 500 units");
    assert_eq!(validate_args(&"😀".repeat(251)), Err(ArgsError::TooLong));
}

#[test]
fn args_never_recurse_or_leak_context() {
    // ARG-09: argument text is never re-scanned — braces and key names
    // render literally, and no substituted username appears.
    let hostile = "{args} {user} {{args}} {username}";
    let out = render_template("Says {args}", &ctx_with_args(hostile)).expect("renders");
    assert_eq!(out, format!("Says {hostile}"));
    assert!(
        !out.contains("tester"),
        "argument braces must not expand context: {out}"
    );
}

#[test]
fn args_render_past_ceiling_refuses_whole() {
    // ARG-10: the existing rendered-output bound applies after substitution
    // and refuses the whole render — no partial send.
    let args = "y".repeat(MAX_RENDERED_CHARS);
    assert_eq!(validate_args(&args), Err(ArgsError::TooLong));
    let ctx = ctx_with_args(&"z".repeat(1996));
    assert!(
        matches!(
            render_template("Says {args}", &ctx),
            Err(TemplateError::RenderTooLong(_))
        ),
        "1996-unit args plus literal prefix must refuse whole"
    );
}

#[test]
fn repeated_args_get_the_same_value_in_one_pass() {
    // ARG-18: every occurrence receives the same value, single pass.
    assert_eq!(
        render_template("{args} and {args}", &ctx_with_args("same")),
        Ok("same and same".to_owned())
    );
}

#[test]
fn args_get_byte_identical_mention_treatment() {
    // ARG-07: hostile mass mentions (including zero-width-split forms) get
    // byte-identical mention treatment to the same text arriving via any
    // existing template input — the channel path neutralizes rendered output
    // regardless of which key carried the text.
    for hostile in [
        "@everyone look",
        "ping @here now",
        "@\u{200b}everyone split",
    ] {
        let via_args = render_template("Says {args}", &ctx_with_args(hostile)).expect("renders");
        let via_user = TemplateContext {
            user: hostile.to_owned(),
            ..ctx_with_args("")
        };
        let via_user = render_template("Says {user}", &via_user).expect("renders");
        assert_eq!(
            neutralize_mentions(&via_args),
            neutralize_mentions(&via_user),
            "differential mention treatment for {hostile:?}"
        );
        assert!(
            !via_args.contains("@everyone") || neutralize_mentions(&via_args) != via_args,
            "raw mass mention must not survive neutralization: {via_args:?}"
        );
    }
}

#[test]
fn pills_and_links_render_inert_as_member_text() {
    // ARG-08 (core half): pills and links substitute literally; mention
    // parsing stays disabled at the REST boundary (`parse: []`), so they
    // render as inert text exactly as a member message would carry them.
    let pills = "<@1234> <@&5678> <#9012> https://example.invalid/x";
    let out = render_template("Says {args}", &ctx_with_args(pills)).expect("renders");
    assert_eq!(out, format!("Says {pills}"));
    assert_eq!(
        neutralize_mentions(&out),
        out,
        "pills carry no mass mention to neutralize"
    );
}

#[test]
fn empty_render_refuses_preview_in_fixed_copy() {
    // ARG-20 (core half): an `{args}`-only template with empty arguments
    // renders no message text, which run paths refuse — so both previews say
    // so in fixed copy instead of showing a blank body.
    use two_bot_core::custom_commands::{
        format_draft_preview, format_inspect_preview, StoredCommand,
    };
    let row = StoredCommand {
        guild_id: "2222".to_owned(),
        name: "faq".to_owned(),
        description: "FAQ".to_owned(),
        template: "{args}".to_owned(),
        text_trigger: Some("!faq".to_owned()),
        enabled: true,
    };
    let ctx = ctx_with_args("");
    assert_eq!(
        format_inspect_preview(&row, &ctx),
        "Template for /faq renders no message text with these arguments. Add text around {args} with /command name:faq template:..."
    );
    assert_eq!(
        format_draft_preview("{args}", &ctx),
        "Draft renders no message text with these arguments. Add text around {args} and retry /command-inspect draft:Hello. Nothing was saved or published."
    );
}
