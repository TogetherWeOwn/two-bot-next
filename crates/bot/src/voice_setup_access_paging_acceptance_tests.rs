//! Protected acceptance: browse complete `/setup` creator and `/access show`
//! restriction output without truncation.
//!
//! Consumer: server admins inspecting creator channels and command
//! restrictions. User value: reach every creator/restriction and still see
//! health warnings, instead of losing the end of a long private response.
//!
//! Baseline RED: `/setup` renders every creator in one unbounded panel
//! (`setup_panel`), and `/access show` plus every post-save confirmation
//! renders every restriction (`access_summary`). There is no read paging,
//! no `page` option in the registry, and no continuation for an oversized
//! row. These tests pin the paged contract through real dispatch
//! (`voice_interaction` + `handle_capture`), the real store doubles, and
//! the published registry — not source strings.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on
//! the acceptance card):
//!
//! - `/setup` gains an optional Integer `page` (min 1, default 1) and
//!   `/access show` gains the same nested option. No other command or
//!   subcommand gains `page`; no new command, setting, or surface.
//! - Every non-empty read page is bounded to 2000 UTF-16 units *before*
//!   the final safety clip, carries `Page {n} of {total}`, preserves every
//!   creator ID, command restriction, and role ID across pages exactly
//!   once in deterministic order (creators by channel ID ascending,
//!   restrictions by command name ascending), never clips an ID/mention
//!   token, and names a copyable next page (`/setup page:{n+1}`,
//!   `/access show page:{n+1}`). Re-requesting a page is byte-identical.
//! - Every `/setup` page keeps worker status visible (`running`/`paused`,
//!   `Tracked rooms:`, failure lines) and every `/access show` page keeps
//!   `Room creation:` + `Required role:`. A store read failure stays
//!   honest (`Could not load creator channels…`, `Could not read the
//!   voice-room settings…`).
//! - One restriction larger than a page splits into explicitly labeled
//!   deterministic continuation segments (each page names the command and
//!   carries `part`/`continu…`), not an impossible whole-row promise.
//! - Strict page decoding: 0, negative, overflow, wrong-typed, and
//!   duplicate `page` values refuse with a short (≤500 units) reply naming
//!   `page`, the remedy (`/setup`, `/access show`), and a valid start
//!   (`1`), with no rows and no mutation. Past-end pages refuse the same
//!   way; refusal is never empty success.
//! - Member `/setup` stays the same generic status with no IDs, settings,
//!   failure classes, store errors, or `Page` metadata; a forged `page`
//!   never enters the admin/store path. `/access` denies non-admins before
//!   page validation. Reads never save, never call `on_saved`, and never
//!   mutate actor/config/audit state.
//! - Post-save `/access` replies stay `Saved`, stay bounded (≤600 units),
//!   point at the read view (`/access show`), and never embed the full
//!   restriction list — while save semantics (validation, errors, stored
//!   update) stay intact.
//! - Responses are mention-safe (no raw `@everyone`/`@here`) and never
//!   rely on the 2000-unit clip as pagination.
//! - `docs/commands.md` is regenerated from the registry (via
//!   `crates/core/tests/reference_docs.rs`) so `/setup` shows `page` and
//!   `/access show` shows `show page`.
//!
//! No production/staging credentials or effects; in-memory `Store`/`Http`
//! doubles only, no disposable database needed. Failing on baseline is the
//! honest RED: unbounded single responses with no `Page` metadata.

#![cfg(test)]

use super::*;

const MAX_ROLE: u64 = u64::MAX;
const CONTENT_BOUND: usize = 2000;
const REFUSAL_BOUND: usize = 500;
const POST_SAVE_BOUND: usize = 600;
const MANY_CREATORS: usize = 120;

fn utf16_len(text: &str) -> usize {
    two_bot_core::message_safety::text_len(text)
}

fn page_int(value: i64) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value: CommandOptionValue::Integer(value),
    }
}

fn page_value(value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value,
    }
}

fn setup_admin(options: Vec<CommandDataOption>) -> Interaction {
    voice_interaction(
        Some(command_data("setup", options)),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

fn setup_member(options: Vec<CommandDataOption>, permissions: Option<Permissions>) -> Interaction {
    voice_interaction(Some(command_data("setup", options)), permissions, true)
}

fn show_admin(args: Vec<CommandDataOption>) -> Interaction {
    access_interaction(sub_option("show", args), true)
}

fn show_member(args: Vec<CommandDataOption>) -> Interaction {
    access_interaction(sub_option("show", args), false)
}

fn page_meta(text: &str) -> Option<(usize, usize)> {
    let tail = text.split_once("Page ")?.1;
    let (current, total) = tail.split_once(" of ")?;
    let total: String = total.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((current.parse().ok()?, total.parse().ok()?))
}

fn setup_ids(text: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("- <#") {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("- <#") else {
            continue;
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(id) = digits.parse::<u64>() {
            // Only count complete mention tokens (`<#id>`).
            if rest[digits.len()..].starts_with('>') {
                ids.push(id);
            }
        }
    }
    ids
}

fn access_command_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("- /") else {
            continue;
        };
        let end = rest.find([':', ' ', '(']).unwrap_or(rest.len());
        if end > 0 {
            names.push(rest[..end].to_owned());
        }
    }
    names
}

fn access_role_ids(text: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@&") {
        rest = &rest[start + "<@&>".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        if let Ok(id) = digits.parse::<u64>() {
            if rest[digits.len()..].starts_with('>') {
                ids.push(id);
            }
        }
    }
    ids
}

fn assert_bounded(text: &str, what: &str) {
    assert!(
        utf16_len(text) <= CONTENT_BOUND,
        "{what} exceeds {CONTENT_BOUND} UTF-16 units (got {}): {text:.300}",
        utf16_len(text)
    );
}

fn assert_no_clipped_tokens(text: &str, what: &str) {
    for marker in ["<#", "<@&"] {
        let mut rest = text;
        while let Some(start) = rest.find(marker) {
            let after = &rest[start + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            assert!(
                !digits.is_empty(),
                "{what}: {marker} without an ID near {start}: {text:.300}"
            );
            assert!(
                after[digits.len()..].starts_with('>'),
                "{what}: clipped {marker}{digits} token (missing `>`): {text:.300}"
            );
            rest = &after[digits.len()..];
        }
    }
    assert!(
        !text.ends_with("<#") && !text.ends_with("<@&"),
        "{what}: response ends mid-token: {text:.200}"
    );
}

fn assert_mention_safe(text: &str, what: &str) {
    assert!(
        !text.contains("@everyone") && !text.contains("@here"),
        "{what}: raw mass mention must be neutralized: {text:.300}"
    );
}

fn assert_setup_refusal(text: &str, what: &str) {
    assert!(
        utf16_len(text) <= REFUSAL_BOUND,
        "{what}: refusal must be short (≤{REFUSAL_BOUND} units, got {}): {text}",
        utf16_len(text)
    );
    assert!(
        text.to_ascii_lowercase().contains("page"),
        "{what}: refusal must explain the bad page: {text}"
    );
    assert!(
        text.contains("/setup"),
        "{what}: refusal must name the remedy: {text}"
    );
    assert!(
        text.contains('1'),
        "{what}: refusal must give a valid starting page: {text}"
    );
    assert!(
        !text.contains("<#"),
        "{what}: refusal must not serve creator rows: {text:.300}"
    );
    assert!(
        !text.contains("Creator channels (") && !text.contains("No creator channels yet."),
        "{what}: out-of-range is not success: {text:.300}"
    );
    assert_mention_safe(text, what);
}

fn assert_access_refusal(text: &str, what: &str) {
    assert!(
        utf16_len(text) <= REFUSAL_BOUND,
        "{what}: refusal must be short (≤{REFUSAL_BOUND} units, got {}): {text}",
        utf16_len(text)
    );
    assert!(
        text.to_ascii_lowercase().contains("page"),
        "{what}: refusal must explain the bad page: {text}"
    );
    assert!(
        text.contains("/access"),
        "{what}: refusal must name the remedy: {text}"
    );
    assert!(
        text.contains('1'),
        "{what}: refusal must give a valid starting page: {text}"
    );
    assert!(
        !text
            .trim_start()
            .lines()
            .any(|line| line.trim_start().starts_with("- /")),
        "{what}: refusal must not serve restriction rows: {text:.300}"
    );
    assert!(
        !text.contains("Restricted commands:"),
        "{what}: out-of-range is not success: {text:.300}"
    );
    assert!(
        !text.contains("<@&"),
        "{what}: refusal must not leak role mentions: {text:.300}"
    );
    assert_mention_safe(text, what);
}

fn many_creators_desc(count: usize) -> Vec<CreatorChannel> {
    // Reverse insertion order: paging must still render channel-ID ascending.
    (0..count)
        .rev()
        .map(|index| CreatorChannel::new(GUILD, 10_000 + index as u64))
        .collect()
}

fn setup_runtime_with(
    creators: Vec<CreatorChannel>,
    creators_error: Option<StoreError>,
) -> (
    VoiceRuntime<Store, Http>,
    std::sync::Arc<std::sync::Mutex<Vec<CreatorChannel>>>,
    Trace,
) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(creators));
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let shared = shared.clone();
            let trace = trace.clone();
            move || {
                let mut store = Store::new(trace.clone());
                store.creators = shared.clone();
                store.creators_error = creators_error;
                (store, Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        true,
    );
    (runtime, shared, trace)
}

async fn setup_text(runtime: &VoiceRuntime<Store, Http>, interaction: &Interaction) -> String {
    let (owned, response) = handle_capture(runtime, interaction).await;
    assert!(owned, "setup must stay owned");
    response_text(response.as_ref().expect("setup reply"))
}

async fn show_text(runtime: &VoiceRuntime<Store, Http>, interaction: &Interaction) -> String {
    let (owned, response) = handle_capture(runtime, interaction).await;
    assert!(owned, "access must stay owned");
    response_text(response.as_ref().expect("access reply"))
}

fn maximal_controls() -> AccessControls {
    // Every supported command restricted; maximal role IDs force multi-page
    // output. One entry stays empty to pin `admins only` across paging.
    let commands = two_bot_core::voice_access::VOICE_COMMANDS;
    assert!(!commands.is_empty(), "voice command list must exist");
    let mut controls = AccessControls::default();
    for (index, command) in commands.iter().enumerate() {
        if *command == "template" {
            controls
                .command_roles
                .insert((*command).to_owned(), Vec::new());
            continue;
        }
        let base = MAX_ROLE - (index as u64 * 3);
        controls
            .command_roles
            .insert((*command).to_owned(), vec![base, base - 1, base - 2]);
    }
    assert_eq!(controls.command_roles.len(), commands.len());
    two_bot_core::voice_access::validate_access_controls(&controls)
        .expect("maximal fixture must validate");
    controls
}

#[test]
fn registry_adds_optional_page_only_to_setup_and_access_show() {
    let defs = voice_command_set(&VoiceGates { enabled: true });
    assert_eq!(defs.len(), 22, "no new commands: {defs:?}");
    let integer = two_bot_core::commands::CommandOptionType::Integer.as_u8();

    let setup = defs
        .iter()
        .find(|def| def.name == "setup")
        .expect("setup stays published");
    assert!(
        setup.description.contains("Show"),
        "setup meaning stays: {}",
        setup.description
    );
    assert_eq!(
        setup.options.len(),
        1,
        "setup gains exactly the page option: {:?}",
        setup.options
    );
    let page = &setup.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(page.kind, integer, "page is an integer");
    assert_ne!(page.required, Some(true), "page stays optional");
    assert_eq!(page.min_value, Some(1), "page is strictly positive");
    assert!(
        page.description.to_ascii_lowercase().contains("page"),
        "page describes itself: {}",
        page.description
    );

    let access = defs
        .iter()
        .find(|def| def.name == "access")
        .expect("access stays published");
    for sub in &access.options {
        if sub.name == "show" {
            assert_eq!(
                sub.options.len(),
                1,
                "access show gains exactly the page option: {:?}",
                sub.options
            );
            let page = &sub.options[0];
            assert_eq!(page.name, "page");
            assert_eq!(page.kind, integer);
            assert_ne!(page.required, Some(true));
            assert_eq!(page.min_value, Some(1));
            assert!(
                page.description.to_ascii_lowercase().contains("page"),
                "show page describes itself: {}",
                page.description
            );
        } else {
            assert!(
                !sub.options.iter().any(|opt| opt.name == "page"),
                "only the read path gains page ({} must not): {:?}",
                sub.name,
                sub.options
            );
        }
    }
    assert!(
        access.options.iter().any(|sub| sub.name == "show"),
        "access show stays published"
    );

    for def in &defs {
        if def.name == "setup" || def.name == "access" {
            continue;
        }
        assert!(
            !def.options.iter().any(|opt| opt.name == "page")
                && !def
                    .options
                    .iter()
                    .flat_map(|opt| opt.options.iter())
                    .any(|opt| opt.name == "page"),
            "no new paging surface on /{}: {:?}",
            def.name,
            def.options
        );
    }
}

#[test]
fn reference_docs_regenerate_the_new_page_options() {
    let path = format!("{}/../../docs/commands.md", env!("CARGO_MANIFEST_DIR"));
    let docs = std::fs::read_to_string(&path).expect("docs/commands.md is readable");
    let section = |header: &str| {
        let start = docs
            .find(header)
            .unwrap_or_else(|| panic!("{header} stays documented"));
        let rest = &docs[start..];
        let end = rest[header.len()..]
            .find("\n## `")
            .map_or(rest.len(), |index| header.len() + index);
        rest[..end].to_owned()
    };
    let setup = section("## `/setup`");
    assert!(
        setup.contains("`page` | integer | false | 1"),
        "regenerated /setup documents the page option: {setup}"
    );
    let access = section("## `/access`");
    assert!(
        access.contains("`show page` | integer | false | 1"),
        "regenerated /access documents show page: {access}"
    );
}

#[tokio::test]
async fn setup_empty_and_single_cover_page_one_and_refuse_page_two() {
    for creators in [Vec::new(), vec![CreatorChannel::new(GUILD, CREATOR)]] {
        let expected = creators.len();
        let (runtime, shared, _) = setup_runtime_with(creators, None);
        let before = shared.lock().unwrap().clone();
        let default = setup_text(&runtime, &setup_admin(Vec::new())).await;
        let first = setup_text(&runtime, &setup_admin(vec![page_int(1)])).await;
        let second = setup_text(&runtime, &setup_admin(vec![page_int(2)])).await;
        let after = shared.lock().unwrap().clone();
        assert_eq!(before, after, "reads never mutate creators");
        assert_bounded(&default, "setup default");
        assert_bounded(&first, "setup page 1");
        assert_no_clipped_tokens(&default, "setup default");
        assert_mention_safe(&default, "setup default");
        assert_eq!(
            default, first,
            "omitted page is byte-identical to page 1 (n={expected})"
        );
        if expected == 0 {
            assert!(
                default.contains("No creator channels yet."),
                "empty stays discoverable: {default}"
            );
            assert!(setup_ids(&default).is_empty());
        } else {
            assert_eq!(setup_ids(&default), vec![CREATOR]);
            assert!(
                default.contains(&format!("<#{CREATOR}>")),
                "single creator stays visible: {default}"
            );
        }
        // Worker status stays visible even without paging.
        assert!(
            default.contains("Voice rooms are running.") || default.contains("paused"),
            "worker status stays: {default}"
        );
        assert!(
            default.contains("Tracked rooms:"),
            "room count stays: {default}"
        );
        assert_setup_refusal(&second, "setup page 2 of a short list");
    }
}

#[tokio::test]
async fn setup_many_pages_cover_every_creator_exactly_once_in_order() {
    let (runtime, shared, trace) = setup_runtime_with(many_creators_desc(MANY_CREATORS), None);
    let before = shared.lock().unwrap().clone();
    let default = setup_text(&runtime, &setup_admin(Vec::new())).await;
    let first = setup_text(&runtime, &setup_admin(vec![page_int(1)])).await;
    assert_eq!(default, first, "omitted page is byte-identical to page 1");
    let (_, total) = page_meta(&first).expect("page/total must be visible");
    assert!(total > 1, "120 creators cannot fit one bounded reply");
    assert_eq!(page_meta(&first), Some((1, total)), "first page metadata");
    let expected: Vec<u64> = (0..MANY_CREATORS as u64)
        .map(|index| 10_000 + index)
        .collect();
    let mut covered = Vec::new();
    for number in 1..=total {
        let interaction = setup_admin(vec![page_int(number as i64)]);
        let text = setup_text(&runtime, &interaction).await;
        assert_bounded(&text, &format!("setup page {number}"));
        assert_no_clipped_tokens(&text, &format!("setup page {number}"));
        assert_mention_safe(&text, &format!("setup page {number}"));
        assert_eq!(
            page_meta(&text),
            Some((number, total)),
            "stable page totals"
        );
        let ids = setup_ids(&text);
        assert!(!ids.is_empty(), "no empty intermediate page");
        covered.extend(ids);
        // Health context stays independent of creator pagination.
        assert!(
            text.contains("Voice rooms are running.") || text.contains("paused"),
            "worker status stays on page {number}: {text:.200}"
        );
        assert!(
            text.contains("Tracked rooms:"),
            "room count stays on page {number}"
        );
        if number < total {
            assert!(
                text.contains(&format!("/setup page:{}", number + 1)),
                "copyable next page on {number}: {text:.300}"
            );
        } else {
            assert!(
                !text.contains(&format!("/setup page:{}", total + 1)),
                "no nonexistent next page"
            );
        }
        // Re-requesting is byte-identical (deterministic selection).
        let again = setup_text(&runtime, &setup_admin(vec![page_int(number as i64)])).await;
        assert_eq!(text, again, "page {number} is stable");
    }
    assert_eq!(
        covered, expected,
        "channel-ID ascending; every creator exactly once"
    );
    let past_end = setup_text(&runtime, &setup_admin(vec![page_int(total as i64 + 1)])).await;
    assert_setup_refusal(&past_end, "setup past-end");
    assert_eq!(
        *shared.lock().unwrap(),
        before,
        "paging never mutates creators"
    );
    assert!(
        trace.lock().unwrap().is_empty(),
        "reads make no Discord/store writes"
    );
}

#[tokio::test]
async fn setup_rejects_invalid_duplicate_and_overflow_pages_without_mutation() {
    let (runtime, shared, _) = setup_runtime_with(vec![CreatorChannel::new(GUILD, CREATOR)], None);
    let before = shared.lock().unwrap().clone();
    let invalid: Vec<Vec<CommandDataOption>> = vec![
        vec![page_int(0)],
        vec![page_int(-1)],
        vec![page_int(i64::MIN)],
        vec![page_int(i64::MAX)],
        vec![page_int(50)],
        vec![page_value(CommandOptionValue::String("2".to_owned()))],
        vec![page_value(CommandOptionValue::String(
            "@everyone invalid-page".to_owned(),
        ))],
        vec![page_value(CommandOptionValue::String(String::new()))],
        vec![page_value(CommandOptionValue::Boolean(true))],
        vec![page_value(CommandOptionValue::Boolean(false))],
        vec![page_value(CommandOptionValue::Number(1.5))],
        vec![page_value(CommandOptionValue::Number(1.0))],
        vec![page_value(CommandOptionValue::SubCommand(Vec::new()))],
        vec![page_int(1), page_int(2)],
        vec![page_int(1), page_int(1)],
        vec![
            page_int(1),
            page_value(CommandOptionValue::String("2".to_owned())),
        ],
    ];
    for (index, options) in invalid.into_iter().enumerate() {
        let text = setup_text(&runtime, &setup_admin(options)).await;
        assert_setup_refusal(&text, &format!("setup invalid page #{index}"));
        assert!(
            !text.contains("invalid-page"),
            "never echo malformed input: {text}"
        );
    }
    assert_eq!(
        *shared.lock().unwrap(),
        before,
        "refusals never mutate creators"
    );
}

#[tokio::test]
async fn setup_member_view_ignores_forged_pages_and_reads_no_store() {
    // Failing store: a member must still see the generic view with no leak,
    // and every forged page must answer identically without store cost.
    let (runtime, _, _) = setup_runtime_with(many_creators_desc(30), Some(StoreError::Unavailable));
    for permissions in [
        Some(Permissions::empty()),
        Some(Permissions::VIEW_CHANNEL | Permissions::MANAGE_GUILD),
        None,
    ] {
        let default = setup_text(&runtime, &setup_member(Vec::new(), permissions)).await;
        assert!(
            default.contains("Voice rooms are running."),
            "member stays generic: {default}"
        );
        for leaked in [
            "<#",
            "store",
            "Unavailable",
            "unavailable",
            "Could not load",
            "Creator channels",
            "Tracked rooms",
            "failures",
            "Page ",
            "/setup page:",
        ] {
            assert!(
                !default.contains(leaked),
                "{leaked:?} leaked to a member: {default}"
            );
        }
        assert!(
            !default.chars().any(|c| c.is_ascii_digit()),
            "member view carries no IDs: {default}"
        );
        for options in [
            vec![page_int(1)],
            vec![page_int(2)],
            vec![page_int(0)],
            vec![page_int(i64::MAX)],
            vec![page_value(CommandOptionValue::String("2".to_owned()))],
            vec![page_int(1), page_int(2)],
        ] {
            let forged = setup_text(&runtime, &setup_member(options, permissions)).await;
            assert_eq!(
                forged, default,
                "member page input never enters the admin/store path"
            );
        }
    }
}

#[tokio::test]
async fn setup_admin_store_errors_stay_honest_and_bounded() {
    for error in [StoreError::Unavailable, StoreError::CredentialRefused] {
        let display = error.to_string();
        let (runtime, _, _) = setup_runtime_with(Vec::new(), Some(error));
        let default = setup_text(&runtime, &setup_admin(Vec::new())).await;
        let first = setup_text(&runtime, &setup_admin(vec![page_int(1)])).await;
        assert_eq!(default, first, "error view still defaults to page 1");
        assert_bounded(&default, "setup store-error");
        assert!(
            default.contains("Could not load creator channels"),
            "honest read failure: {default}"
        );
        assert!(
            default.contains(&display),
            "plain-words cause ({display}): {default}"
        );
        assert!(
            !default.contains("Unavailable"),
            "variant name stays hidden: {default}"
        );
        assert!(setup_ids(&default).is_empty());
        let second = setup_text(&runtime, &setup_admin(vec![page_int(2)])).await;
        assert_setup_refusal(&second, "setup page 2 with no creators");
    }
}

#[tokio::test]
async fn access_show_pages_cover_every_restriction_with_maximal_roles() {
    let controls = maximal_controls();
    let expected_order: Vec<String> = {
        let mut names: Vec<String> = controls.command_roles.keys().cloned().collect();
        names.sort();
        names
    };
    let expected_roles: std::collections::BTreeMap<String, Vec<u64>> = controls
        .command_roles
        .iter()
        .map(|(command, roles)| (command.clone(), roles.clone()))
        .collect();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let before = shared.lock().unwrap().clone();

    let default = show_text(&runtime, &show_admin(Vec::new())).await;
    let first = show_text(&runtime, &show_admin(vec![page_int(1)])).await;
    assert_eq!(default, first, "omitted page is byte-identical to page 1");
    let (_, total) = page_meta(&first).expect("page/total must be visible");
    assert!(
        total > 1,
        "26 maximal restrictions cannot fit one bounded reply"
    );
    let mut seen_names = Vec::new();
    let mut seen_roles: std::collections::BTreeMap<String, Vec<u64>> =
        std::collections::BTreeMap::new();
    for number in 1..=total {
        let text = show_text(&runtime, &show_admin(vec![page_int(number as i64)])).await;
        assert_bounded(&text, &format!("access page {number}"));
        assert_no_clipped_tokens(&text, &format!("access page {number}"));
        assert_mention_safe(&text, &format!("access page {number}"));
        assert_eq!(
            page_meta(&text),
            Some((number, total)),
            "stable page totals"
        );
        // Creation/required-role context stays on every page.
        assert!(
            text.contains("Room creation:"),
            "creation context stays on page {number}: {text:.200}"
        );
        assert!(
            text.contains("Required role:"),
            "required-role context stays on page {number}"
        );
        let names = access_command_names(&text);
        assert!(!names.is_empty(), "no empty intermediate page");
        seen_names.extend(names.clone());
        for name in names {
            let line = text
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("- /{name}")))
                .unwrap_or_else(|| panic!("row for /{name} on page {number}: {text:.300}"));
            if expected_roles[&name].is_empty() {
                assert!(
                    line.contains("admins only"),
                    "empty roles fail closed on page {number}: {line}"
                );
            } else {
                let mut line_roles = Vec::new();
                let mut rest = line;
                while let Some(start) = rest.find("<@&") {
                    rest = &rest[start + "<@&>".len()..];
                    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let id: u64 = digits.parse().expect("role digits");
                    assert!(
                        rest[digits.len()..].starts_with('>'),
                        "complete mention token: {line}"
                    );
                    line_roles.push(id);
                }
                assert_eq!(
                    line_roles, expected_roles[&name],
                    "role order preserved for /{name}"
                );
                seen_roles.insert(name.clone(), line_roles);
            }
        }
        if number < total {
            assert!(
                text.contains(&format!("/access show page:{}", number + 1)),
                "copyable next page on {number}: {text:.300}"
            );
        } else {
            assert!(
                !text.contains(&format!("/access show page:{}", total + 1)),
                "no nonexistent next page"
            );
        }
        let again = show_text(&runtime, &show_admin(vec![page_int(number as i64)])).await;
        assert_eq!(text, again, "page {number} is stable");
    }
    assert_eq!(
        seen_names, expected_order,
        "command-name ascending; every restriction exactly once"
    );
    // Maximal IDs survive round-trip with no clipping or duplication.
    let mut all_seen: Vec<u64> = seen_roles.values().flatten().copied().collect();
    all_seen.sort_unstable();
    let mut all_expected: Vec<u64> = expected_roles.values().flatten().copied().collect();
    all_expected.sort_unstable();
    assert_eq!(all_seen, all_expected, "every maximal role ID preserved");
    assert!(
        all_expected.contains(&MAX_ROLE),
        "fixture really uses maximal IDs"
    );
    // Empty-means-admins-only survives paging.
    let mut found_admins_only = false;
    for number in 1..=total {
        let text = show_text(&runtime, &show_admin(vec![page_int(number as i64)])).await;
        if text.contains("- /template: admins only") {
            found_admins_only = true;
        }
    }
    assert!(found_admins_only, "empty restriction stays `admins only`");
    let past_end = show_text(&runtime, &show_admin(vec![page_int(total as i64 + 1)])).await;
    assert_access_refusal(&past_end, "access past-end");
    assert_eq!(*shared.lock().unwrap(), before, "reads never mutate");
}

#[tokio::test]
async fn access_show_empty_and_single_preserve_discovery() {
    // Empty store: page 1 succeeds with the none-row; page 2 refuses.
    let shared = std::sync::Arc::new(std::sync::Mutex::new(AccessControls::default()));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let default = show_text(&runtime, &show_admin(Vec::new())).await;
    let first = show_text(&runtime, &show_admin(vec![page_int(1)])).await;
    assert_eq!(default, first);
    assert_bounded(&default, "access empty");
    assert!(
        default.contains("Restricted commands: none"),
        "empty stays discoverable: {default}"
    );
    assert!(default.contains("Room creation:"));
    assert!(default.contains("Required role:"));
    let second = show_text(&runtime, &show_admin(vec![page_int(2)])).await;
    assert_access_refusal(&second, "access page 2 of empty");

    // Single restriction: exact UTF-16 budgeting still bounds the reply.
    let mut one = AccessControls::default();
    one.command_roles
        .insert("kick".to_owned(), vec![MAX_ROLE, MAX_ROLE - 1]);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(one));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let text = show_text(&runtime, &show_admin(Vec::new())).await;
    assert_bounded(&text, "access single");
    assert_no_clipped_tokens(&text, "access single");
    assert!(
        text.contains(&format!("<@&{MAX_ROLE}>")),
        "maximal ID preserved: {text}"
    );
    assert_eq!(
        show_text(&runtime, &show_admin(vec![page_int(1)])).await,
        text
    );
    assert_access_refusal(
        &show_text(&runtime, &show_admin(vec![page_int(2)])).await,
        "access page 2 of single",
    );
}

#[tokio::test]
async fn access_show_splits_one_oversized_restriction_into_labeled_segments() {
    let mut controls = AccessControls::default();
    let roles: Vec<u64> = (0..100).map(|index| MAX_ROLE - index).collect();
    controls
        .command_roles
        .insert("kick".to_owned(), roles.clone());
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    let first = show_text(&runtime, &show_admin(vec![page_int(1)])).await;
    let (_, total) = page_meta(&first).expect("oversized row must paginate");
    assert!(
        total > 1,
        "one 100-maximal-role row cannot fit one bounded reply"
    );
    let mut covered = Vec::new();
    for number in 1..=total {
        let text = show_text(&runtime, &show_admin(vec![page_int(number as i64)])).await;
        assert_bounded(&text, &format!("oversized page {number}"));
        assert_no_clipped_tokens(&text, &format!("oversized page {number}"));
        assert!(
            text.contains("/kick"),
            "same command recurs across segments (page {number}): {text:.300}"
        );
        let lower = text.to_ascii_lowercase();
        assert!(
            lower.contains("part") || lower.contains("continu"),
            "explicit continuation label on page {number}: {text:.300}"
        );
        assert!(
            text.contains("Room creation:") && text.contains("Required role:"),
            "context stays on continuation page {number}"
        );
        covered.extend(access_role_ids(&text));
    }
    assert_eq!(
        covered, roles,
        "deterministic segments; every role exactly once in order"
    );
}

#[tokio::test]
async fn access_show_rejects_invalid_duplicate_and_overflow_pages_without_saving() {
    let mut controls = AccessControls::default();
    controls.command_roles.insert("kick".to_owned(), vec![7]);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    // A save error that reads must never trip: refusals stay refusals.
    let runtime = shared_runtime(
        Trace::default(),
        shared.clone(),
        Some(StoreError::Unavailable),
    );
    let before = shared.lock().unwrap().clone();
    let invalid: Vec<Vec<CommandDataOption>> = vec![
        vec![page_int(0)],
        vec![page_int(-1)],
        vec![page_int(i64::MIN)],
        vec![page_int(i64::MAX)],
        vec![page_int(50)],
        vec![page_value(CommandOptionValue::String("2".to_owned()))],
        vec![page_value(CommandOptionValue::String(
            "@everyone invalid-page".to_owned(),
        ))],
        vec![page_value(CommandOptionValue::Boolean(true))],
        vec![page_value(CommandOptionValue::Number(1.5))],
        vec![page_value(CommandOptionValue::SubCommand(Vec::new()))],
        vec![page_int(1), page_int(2)],
        vec![page_int(1), page_int(1)],
    ];
    for (index, args) in invalid.into_iter().enumerate() {
        let text = show_text(&runtime, &show_admin(args)).await;
        assert_access_refusal(&text, &format!("access invalid page #{index}"));
        assert!(
            !text.contains("invalid-page"),
            "never echo malformed input: {text}"
        );
        assert!(
            !text.starts_with("Saved"),
            "refusal never saves: {text:.200}"
        );
    }
    assert_eq!(
        *shared.lock().unwrap(),
        before,
        "refusals never mutate or save"
    );
}

#[tokio::test]
async fn access_show_needs_an_admin_before_page_validation() {
    let mut controls = AccessControls::default();
    controls.command_roles.insert("kick".to_owned(), vec![7]);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);
    for args in [
        Vec::new(),
        vec![page_int(1)],
        vec![page_int(2)],
        vec![page_int(0)],
        vec![page_int(i64::MAX)],
        vec![page_value(CommandOptionValue::String("2".to_owned()))],
        vec![page_int(1), page_int(2)],
    ] {
        let (owned, response) = handle_capture(&runtime, &show_member(args)).await;
        assert!(owned);
        let text = response_text(response.as_ref().expect("denial"));
        assert_eq!(
            text, "You need Manage Channels to use /access.",
            "admission precedes paging"
        );
    }
}

#[tokio::test]
async fn access_show_reports_store_failures_honestly_and_saves_nothing() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(AccessControls::default()));
    let runtime = VoiceRuntime::new(
        {
            let shared = shared.clone();
            move || {
                let mut store = Store::new(Trace::default());
                store.access = shared.clone();
                store.access_error = Some(StoreError::Unavailable);
                store.save_access_error = Some(StoreError::Unavailable);
                (store, Http::new(Trace::default()))
            }
        },
        Duration::from_millis(10),
        true,
    );
    let before = shared.lock().unwrap().clone();
    for args in [Vec::new(), vec![page_int(1)]] {
        let text = show_text(&runtime, &show_admin(args)).await;
        assert_eq!(
            text, "Could not read the voice-room settings. Nothing was changed; try again.",
            "honest read failure"
        );
    }
    assert_eq!(
        *shared.lock().unwrap(),
        before,
        "failed reads change nothing"
    );
}

#[tokio::test]
async fn access_post_save_acknowledgements_stay_short_and_point_at_the_read_view() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(maximal_controls()));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);

    let run = |sub: CommandDataOption| {
        let interaction = access_interaction(sub, true);
        let runtime = &runtime;
        async move {
            let (_, response) = handle_capture(runtime, &interaction).await;
            response_text(response.as_ref().expect("access reply"))
        }
    };

    // Restrict overwrites one row; the ack must not embed the other 25.
    let text = run(sub_option(
        "restrict",
        vec![command_option("command", "kick"), role_option("role", 7)],
    ))
    .await;
    assert!(
        text.contains("Saved"),
        "post-save stays truthful: {text:.200}"
    );
    assert!(
        utf16_len(&text) <= POST_SAVE_BOUND,
        "post-save stays short (≤{POST_SAVE_BOUND} units, got {}): {text:.300}",
        utf16_len(&text)
    );
    assert!(
        text.contains("/access show"),
        "post-save points at the read view: {text:.300}"
    );
    assert!(
        !text.contains(&format!("<@&{MAX_ROLE}>")),
        "post-save never embeds the full list: {text:.300}"
    );
    assert_mention_safe(&text, "post-save");
    assert_eq!(
        shared.lock().unwrap().command_roles.get("kick"),
        Some(&vec![7]),
        "save semantics intact"
    );
    // The change is visible through the paged read view.
    let first = show_text(&runtime, &show_admin(vec![page_int(1)])).await;
    let (_, total) = page_meta(&first).expect("read view still pages");
    let mut found = false;
    for number in 1..=total {
        let page = show_text(&runtime, &show_admin(vec![page_int(number as i64)])).await;
        if page.contains("- /kick: <@&7>") {
            found = true;
        }
    }
    assert!(found, "saved row appears in the paged read view");

    // Other mutations share the short-ack shape.
    let text = run(sub_option(
        "unrestrict",
        vec![command_option("command", "limit")],
    ))
    .await;
    assert!(text.contains("Saved"), "{text:.200}");
    assert!(
        utf16_len(&text) <= POST_SAVE_BOUND,
        "unrestrict ack stays short: {text:.300}"
    );
    assert!(text.contains("/access show"), "{text:.300}");
    assert!(
        !shared.lock().unwrap().command_roles.contains_key("limit"),
        "unrestrict still removes"
    );

    let text = run(sub_option(
        "creation",
        vec![CommandDataOption {
            name: "enabled".to_owned(),
            value: CommandOptionValue::Boolean(false),
        }],
    ))
    .await;
    assert!(text.contains("Saved"), "{text:.200}");
    assert!(text.contains("/access show"), "{text:.300}");
    assert!(
        utf16_len(&text) <= POST_SAVE_BOUND,
        "creation ack stays short: {text:.300}"
    );
    assert!(!shared.lock().unwrap().room_creation_enabled);

    let text = run(sub_option("role", vec![role_option("role", 9)])).await;
    assert!(text.contains("Saved"), "{text:.200}");
    assert!(text.contains("/access show"), "{text:.300}");
    assert!(
        utf16_len(&text) <= POST_SAVE_BOUND,
        "role ack stays short: {text:.300}"
    );
    assert_eq!(shared.lock().unwrap().required_role, Some(9));

    // Invalid mutations and save failures keep their honest copy with no ack.
    for sub in [
        sub_option("restrict", vec![command_option("command", "kik")]),
        sub_option("unrestrict", vec![command_option("command", "limit")]),
        sub_option("bogus", vec![]),
    ] {
        let before = shared.lock().unwrap().clone();
        let text = run(sub).await;
        assert!(!text.contains("Saved"), "refusal never acks: {text:.200}");
        assert_eq!(*shared.lock().unwrap(), before);
    }
}
