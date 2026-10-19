//! Acceptance tests: goodbye notices render hostile names inert (pure,
//! offline, no Discord call).
//!
//! Failing-first contract for the username slot of
//! `two_bot_core::goodbye_text`. The slot gets `sanitize_vote_reason`-grade
//! treatment as defined by the four properties on the cards: single-line,
//! mention-neutralized, markdown-escaped, length-capped. Normal names render
//! byte-identical to before.
//!
//! Contract bounds pinned here, each traceable to an existing public
//! contract rather than to the implementation under change:
//!
//! - Mention neutralization is the shared `message_safety` neutralizer:
//!   `@everyone` becomes `@` + U+200B + `everyone`, including zero-width
//!   obfuscations, and `message_safety::contains_mass_mention` reports the
//!   rendered output clean. Mention pills (`<@`, `<#`, `<:`, `<a:`) are split
//!   with U+200B so they render literally.
//! - Inline markdown is backslash-escaped for the vote-reason set
//!   (`\`, backtick, `*`, `_`, `~`, `|`, `[`, `]`); a scanner below proves no
//!   unescaped markdown-active character survives in the slot, so any
//!   superset escaper also passes.
//! - Newlines fold to spaces: the whole notice is one line, so line-leading
//!   markup (headings, quotes, lists, fences) can never start.
//! - The rendered slot is capped at 512 characters — the vote-grade public
//!   bound (`VOTE_KICK_PUBLIC_REASON_LIMIT`) — applied to the escaped output
//!   so the bound holds on the wire, and the full notice always fits
//!   `message_safety::CONTENT_LIMIT`.
//!
//! Exact byte pins are used only where every grade-conforming treatment
//! agrees (normal names, the shared neutralizer's canonical output, simple
//! escapes, the ASCII cap). Everything else asserts the inertness
//! properties above. Link-splitting (`://`, bare domains) is deliberately
//! not pinned: it is outside the four-property grade on the cards, and no
//! input below is shaped so that a Builder who ports it would fail.
//!
//! Current `goodbye_text` interpolates the username raw, so the hostile
//! tests FAIL today for exactly that reason — the raw-interpolation mutant
//! is the code as shipped. The normal-name pins pass today and must keep
//! passing byte-identical. Test files only; no runtime code is touched.

use two_bot_core::goodbye_text;
use two_bot_core::message_safety::{contains_mass_mention, text_len, CONTENT_LIMIT};

/// Vote-grade public bound on the rendered slot, in characters.
const SLOT_CAP: usize = 512;

const BOOKS: &str = "Their messages and voice history stay on the books.";

/// The rendered slot between the template's bold markers. Uses the LAST
/// `** left the server` so a hostile name echoing the template text cannot
/// shift the boundary.
fn slot(out: &str) -> &str {
    assert!(out.starts_with("**"), "notice must open bold: {out:?}");
    assert!(out.ends_with(BOOKS), "notice must keep its tail: {out:?}");
    let Some(pos) = out.rfind("** left the server") else {
        panic!("notice must keep the template close: {out:?}");
    };
    assert!(pos >= 2, "slot boundary before opening bold: {out:?}");
    &out[2..pos]
}

/// No markdown-active character from the vote-reason escape set may survive
/// unescaped in the slot. A backslash escapes the next character.
fn assert_no_unescaped_markdown(rendered_slot: &str) {
    let mut escaped = false;
    for ch in rendered_slot.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        assert!(
            !matches!(ch, '`' | '*' | '_' | '~' | '|' | '[' | ']'),
            "unescaped markdown {ch:?} survives in slot {rendered_slot:?}"
        );
    }
}

/// The full inertness battery every hostile render must satisfy.
fn assert_inert(out: &str) {
    assert!(!out.contains('\n'), "notice must be single-line: {out:?}");
    assert!(!out.contains('\r'), "notice must be single-line: {out:?}");
    assert!(
        !contains_mass_mention(out),
        "notice carries a live mass mention: {out:?}"
    );
    assert!(!out.contains("@everyone"), "raw @everyone: {out:?}");
    assert!(!out.contains("@here"), "raw @here: {out:?}");
    for pill in ["<@", "<#", "<:", "<a:"] {
        assert!(!out.contains(pill), "live mention pill {pill:?}: {out:?}");
    }
    assert!(
        text_len(out) <= CONTENT_LIMIT,
        "notice exceeds the Discord content bound: {} chars",
        text_len(out)
    );
    let rendered = slot(out);
    assert!(
        rendered.chars().count() <= SLOT_CAP,
        "slot exceeds the {SLOT_CAP}-char cap: {} chars",
        rendered.chars().count()
    );
    assert_no_unescaped_markdown(rendered);
}

// --- Pins that pass today: normal names are byte-identical ---------------

#[test]
fn normal_names_render_byte_identical() {
    assert_eq!(
        goodbye_text("dave", Some(3)),
        "**dave** left the server (was here 3 days). Their messages and voice history stay on the books."
    );
    assert_eq!(
        goodbye_text("sam", Some(1)),
        "**sam** left the server (was here 1 day). Their messages and voice history stay on the books."
    );
    assert_eq!(
        goodbye_text("kim", Some(0)),
        "**kim** left the server (was here less than a day). Their messages and voice history stay on the books."
    );
    assert_eq!(
        goodbye_text("lee", None),
        "**lee** left the server. Their messages and voice history stay on the books."
    );
}

#[test]
fn normal_unicode_and_punctuation_names_pass_through_unchanged() {
    // None of these carry a mass mention, pill, markdown-active character,
    // newline or overlong run, so the slot must be the name verbatim —
    // including multilingual joiners and a non-mention `@`, which the shared
    // safety contract explicitly preserves.
    for name in [
        "space cadet",
        "mary-jane",
        "o'brien",
        "user42",
        "界界",
        "👩\u{200d}💻",
        "می\u{200c}روم",
        "@user\u{200c}name",
    ] {
        assert_eq!(
            goodbye_text(name, None),
            format!("**{name}** left the server. {BOOKS}"),
            "normal name must render byte-identical: {name:?}"
        );
    }
}

#[test]
fn empty_name_renders_an_empty_slot() {
    assert_eq!(
        goodbye_text("", None),
        format!("**** left the server. {BOOKS}")
    );
}

// --- RED today: hostile names must render inert --------------------------

#[test]
fn mass_mentions_are_neutralized_exactly() {
    assert_eq!(
        goodbye_text("@everyone", None),
        format!("**@\u{200b}everyone** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("@here", None),
        format!("**@\u{200b}here** left the server. {BOOKS}")
    );
    assert_inert(&goodbye_text("@everyone @here", Some(2)));
}

#[test]
fn zero_width_obfuscated_mass_mentions_are_neutralized() {
    for separator in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        let disguised = format!("@{separator}everyone");
        let out = goodbye_text(&disguised, None);
        assert_eq!(
            out,
            format!("**@\u{200b}everyone** left the server. {BOOKS}"),
            "obfuscated mention must canonicalize inert: {disguised:?}"
        );
        assert_inert(&out);
    }
}

#[test]
fn mention_pills_are_split_exactly() {
    assert_eq!(
        goodbye_text("<@44> <@&77>", Some(2)),
        format!("**<\u{200b}@44> <\u{200b}@&77>** left the server (was here 2 days). {BOOKS}")
    );
    assert_eq!(
        goodbye_text("<#13>", None),
        format!("**<\u{200b}#13>** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("<:wave:99>", None),
        format!("**<\u{200b}:wave:99>** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("<a:wave:99>", None),
        format!("**<\u{200b}a:wave:99>** left the server. {BOOKS}")
    );
}

#[test]
fn markdown_bold_and_code_are_escaped_exactly() {
    assert_eq!(
        goodbye_text("**pwned**", None),
        format!(r"**\*\*pwned\*\*** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("`code`", None),
        format!(r"**\`code\`** left the server. {BOOKS}")
    );
    assert_inert(&goodbye_text("***x*** ```y```", None));
}

#[test]
fn markdown_italic_spoiler_strike_and_link_chars_are_escaped() {
    assert_eq!(
        goodbye_text("_under_", None),
        format!(r"**\_under\_** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("||secret||", None),
        format!(r"**\|\|secret\|\|** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("~~gone~~", None),
        format!(r"**\~\~gone\~\~** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text("[click](target)", None),
        format!(r"**\[click\](target)** left the server. {BOOKS}")
    );
}

#[test]
fn backslashes_are_escaped_so_escapes_cannot_be_forged() {
    // A raw `\*` in the slot reads as an escape to Discord; the backslash
    // itself must be escaped so an attacker cannot pre-forge one.
    assert_eq!(
        goodbye_text(r"a\b", None),
        format!(r"**a\\b** left the server. {BOOKS}")
    );
    assert_eq!(
        goodbye_text(r"\*pwn", None),
        format!(r"**\\\*pwn** left the server. {BOOKS}")
    );
}

#[test]
fn newlines_fold_to_a_single_line_exactly() {
    assert_eq!(
        goodbye_text("line1\nline2", None),
        format!("**line1 line2** left the server. {BOOKS}")
    );
}

#[test]
fn crlf_and_cr_fold_to_a_single_line() {
    let out = goodbye_text("a\r\nb\rc", None);
    assert_inert(&out);
    let rendered = slot(&out);
    assert!(rendered.contains('a'), "fold keeps the text: {out:?}");
    assert!(rendered.contains('b'), "fold keeps the text: {out:?}");
    assert!(rendered.contains('c'), "fold keeps the text: {out:?}");
}

#[test]
fn newline_heading_quote_list_and_fence_battery_renders_single_line() {
    let out = goodbye_text("x\n# fake heading\n> quote\n- item\n```code```", None);
    assert_inert(&out);
    assert!(
        out.contains("fake heading"),
        "text stays readable, just inert: {out:?}"
    );
}

#[test]
fn hostile_composite_from_the_roadmap_renders_inert() {
    let out = goodbye_text("**pwned**\n# fake heading\n@everyone", None);
    assert_eq!(
        out,
        format!("**\\*\\*pwned\\*\\* # fake heading @\u{200b}everyone** left the server. {BOOKS}")
    );
    assert_inert(&out);
}

#[test]
fn hostile_name_with_days_keeps_the_stay_text() {
    let out = goodbye_text("@everyone <@44> <@&77>", Some(2));
    assert_inert(&out);
    assert!(
        out.contains("(was here 2 days)"),
        "stay text must survive sanitizing: {out:?}"
    );
}

#[test]
fn overlong_name_is_capped_at_the_vote_grade_limit_exactly() {
    let expected_slot = "a".repeat(SLOT_CAP);
    assert_eq!(
        goodbye_text(&"a".repeat(600), None),
        format!("**{expected_slot}** left the server. {BOOKS}")
    );
    // The wire size does not grow with the input length.
    assert_eq!(
        goodbye_text(&"a".repeat(10_000), None),
        format!("**{expected_slot}** left the server. {BOOKS}")
    );
}

#[test]
fn overlong_markdown_name_stays_capped_after_escaping() {
    // The cap binds the escaped output, so 600 asterisks cannot expand to a
    // 1200-char slot of `\*` pairs.
    let out = goodbye_text(&"*".repeat(600), None);
    assert_inert(&out);
    assert!(
        slot(&out).chars().count() <= SLOT_CAP,
        "escaped slot must respect the cap: {} chars",
        slot(&out).chars().count()
    );
}

#[test]
fn overlong_multibyte_name_is_capped_on_a_char_boundary() {
    for name in ["😀".repeat(600), "界".repeat(600)] {
        let out = goodbye_text(&name, None);
        assert_inert(&out);
        let rendered = slot(&out);
        assert!(
            rendered.chars().count() <= SLOT_CAP,
            "multibyte slot must respect the cap: {} chars",
            rendered.chars().count()
        );
        let first = name.chars().next().unwrap_or('?');
        assert!(
            rendered.chars().all(|ch| ch == first),
            "cap must cut whole scalars, not mangle them: {rendered:?}"
        );
    }
}

#[test]
fn name_of_only_newlines_and_spaces_stays_single_line() {
    for name in ["\n", "\r\n\r\n", "  \n  ", "\n\n\n"] {
        let out = goodbye_text(name, None);
        assert_inert(&out);
    }
}
