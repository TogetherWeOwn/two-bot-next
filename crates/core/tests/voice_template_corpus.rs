//! Runs the independent voice-template corpus (`tests/voice_templates/`)
//! against the V5 naming engine with [`PassthroughExtensions`].
//!
//! The corpus was authored from `docs/voice-rooms.md`, not from this engine.
//! Expected outputs are never edited here: a spec-vs-engine disagreement is a
//! finding for the engine owner, reported as case ID plus expected vs actual.
//!
//! - `exact` cases compare byte-for-byte.
//! - `invariant` cases check nonempty output, the 100-character ceiling,
//!   repeat-render equality, `allowed_outputs` and `casefold_equals`; every
//!   `stability_groups` entry must render the same output across contexts.
//! - `deferred` cases await spec clarification: skipped and counted.
//!
//! Cases that need V6 conditionals or styling are listed in [`PENDING`]. The
//! list may only shrink: a pending case that starts passing fails the test
//! until its ID is removed, and any non-pending case that fails is a
//! regression.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;
use time::{Date, Month};
use two_bot_core::voice_naming::{
    parse, render, resolve_majority_game, ChannelKind, GameOptions, PartyInfo,
    PassthroughExtensions, RoomContext, MAX_NAME_LEN,
};

const CORPUS: &str = include_str!("../../../tests/voice_templates/corpus.json");

/// Non-deferred cases that need the V6 conditional or styling policy. Remove
/// an ID once the engine renders that case as the corpus expects.
const PENDING: &[&str] = &[];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    version: u32,
    #[allow(dead_code)]
    spec: serde_json::Value,
    contexts: BTreeMap<String, CorpusContext>,
    cases: Vec<Case>,
    stability_groups: Vec<StabilityGroup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusContext {
    channel_kind: String,
    number: u32,
    limit: u32,
    #[allow(dead_code)]
    private: bool,
    seed: String,
    owner_id: String,
    original_creator_name: String,
    members: Vec<Member>,
    clock: Clock,
    settings: Settings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    id: String,
    display_name: String,
    nick: Option<String>,
    #[allow(dead_code)]
    roles: Vec<String>,
    game: Option<String>,
    live_discord: bool,
    live_external: bool,
    stream_title: Option<String>,
    party: Option<Party>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Party {
    id: String,
    size: u32,
    maximum: Option<u32>,
    state: String,
    details: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Clock {
    weekday: String,
    month: String,
    hour: u8,
    timezone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    no_game: String,
    aliases: HashMap<String, String>,
    named_lists: HashMap<String, Vec<String>>,
    force_single_game: bool,
    include_inactive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    input: String,
    context: String,
    expected: Expected,
    #[allow(dead_code)]
    covers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Expected {
    Exact {
        output: String,
    },
    Invariant {
        nonempty: bool,
        max_characters: usize,
        stable_for_same_context: bool,
        #[serde(default)]
        allowed_outputs: Option<Vec<String>>,
        #[serde(default)]
        casefold_equals: Option<String>,
    },
    Deferred {
        ambiguity_id: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StabilityGroup {
    id: String,
    case_ids: Vec<String>,
}

/// Adapt the implementation-independent snapshot to the engine's context,
/// following V5 and V7: owner name prefers `/nick`, live means Discord or
/// external, the game is the alias-resolved majority, and parties are
/// deduplicated by ID in first-seen order.
fn room_context(context: &CorpusContext) -> RoomContext {
    let owner = context.members.iter().find(|m| m.id == context.owner_id);
    let live = |m: &Member| m.live_discord || m.live_external;
    let mut parties: Vec<(&str, PartyInfo)> = Vec::new();
    for party in context.members.iter().filter_map(|m| m.party.as_ref()) {
        if !parties.iter().any(|(id, _)| *id == party.id) {
            parties.push((
                &party.id,
                PartyInfo {
                    size: party.size,
                    max: party.maximum,
                    state: party.state.clone(),
                    details: party.details.clone(),
                },
            ));
        }
    }
    let activities: Vec<Option<String>> = context.members.iter().map(|m| m.game.clone()).collect();
    let game_options = GameOptions {
        aliases: context.settings.aliases.clone(),
        force_single: context.settings.force_single_game,
        count_idle_toward_majority: context.settings.include_inactive,
        no_game_label: context.settings.no_game.clone(),
    };
    RoomContext {
        channel_kind: match context.channel_kind.as_str() {
            "temporary" => ChannelKind::Temporary,
            "standalone" | "stage" => ChannelKind::Standalone,
            other => panic!("unknown channel kind {other}"),
        },
        room_number: context.number,
        owner_name: owner
            .map(|m| m.nick.clone().unwrap_or_else(|| m.display_name.clone()))
            .unwrap_or_default(),
        original_creator_name: context.original_creator_name.clone(),
        member_count: context.members.len() as u32,
        owner_present: owner.is_some(),
        live_count: context.members.iter().filter(|m| live(m)).count() as u32,
        user_limit: context.limit,
        game_name: resolve_majority_game(
            &activities,
            owner.and_then(|m| m.game.as_deref()),
            &game_options,
        ),
        stream_title: owner
            .filter(|m| live(m))
            .and_then(|m| m.stream_title.clone())
            .unwrap_or_default(),
        members_playing: context.members.iter().filter(|m| m.game.is_some()).count() as u32,
        parties: parties.into_iter().map(|(_, party)| party).collect(),
        timestamp: civil_timestamp(&context.clock),
        tz_offset_minutes: 0,
        seed: seed(&context.seed),
        named_lists: context.settings.named_lists.clone(),
        fallback_name: String::new(),
    }
}

/// The corpus clock is already guild-local civil time. Pick the first day of
/// that month in 2026 with that weekday, at that hour, as a UTC timestamp.
fn civil_timestamp(clock: &Clock) -> i64 {
    assert_eq!(clock.timezone, "UTC", "corpus clocks are UTC");
    let month = (1..=12)
        .map(|m| Month::try_from(m).expect("month"))
        .find(|m| m.to_string() == clock.month)
        .unwrap_or_else(|| panic!("unknown month {}", clock.month));
    let mut date = Date::from_calendar_date(2026, month, 1).expect("date");
    while date.weekday().to_string() != clock.weekday {
        date = date.next_day().expect("next day");
        assert_eq!(date.month(), month, "unknown weekday {}", clock.weekday);
    }
    date.with_hms(clock.hour, 0, 0)
        .expect("hour")
        .assume_utc()
        .unix_timestamp()
}

/// The corpus seed is opaque; map it consistently with 64-bit FNV-1a.
fn seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn render_case(case: &Case, contexts: &BTreeMap<String, RoomContext>) -> String {
    let context = &contexts[&case.context];
    render(&parse(&case.input), context, &PassthroughExtensions)
}

/// Why an invariant case fails, or `None` when every declared constraint holds.
fn invariant_failure(case: &Case, contexts: &BTreeMap<String, RoomContext>) -> Option<String> {
    let Expected::Invariant {
        nonempty,
        max_characters,
        stable_for_same_context,
        allowed_outputs,
        casefold_equals,
    } = &case.expected
    else {
        unreachable!("invariant case");
    };
    let output = render_case(case, contexts);
    let mut failures = Vec::new();
    if *nonempty && output.is_empty() {
        failures.push("empty output".to_string());
    }
    if output.chars().count() > *max_characters {
        failures.push(format!("over {max_characters} characters"));
    }
    if *stable_for_same_context && render_case(case, contexts) != output {
        failures.push("repeat render differs".to_string());
    }
    if let Some(allowed) = allowed_outputs {
        if !allowed.contains(&output) {
            failures.push(format!("not one of {allowed:?}"));
        }
    }
    if let Some(target) = casefold_equals {
        // Lowercasing equals Unicode case folding for ASCII targets only.
        assert!(target.is_ascii(), "{}: non-ASCII case-fold target", case.id);
        if output.to_lowercase() != *target {
            failures.push(format!(
                "case-folds to {:?}, not {target:?}",
                output.to_lowercase()
            ));
        }
    }
    (!failures.is_empty())
        .then(|| format!("{}: {} (actual {output:?})", case.id, failures.join("; ")))
}

#[test]
fn voice_template_corpus_matches_engine() {
    let corpus: Corpus = serde_json::from_str(CORPUS).expect("corpus parses");
    assert_eq!(corpus.version, 1, "corpus version");
    let contexts: BTreeMap<String, RoomContext> = corpus
        .contexts
        .iter()
        .map(|(name, context)| (name.clone(), room_context(context)))
        .collect();
    let pending: BTreeSet<&str> = PENDING.iter().copied().collect();
    assert_eq!(pending.len(), PENDING.len(), "duplicate PENDING entry");

    let mut ids = BTreeSet::new();
    let (mut exact, mut invariant) = (0usize, 0usize);
    let (mut exact_pass, mut exact_pending, mut invariant_pass, mut invariant_pending) =
        (0usize, 0usize, 0usize, 0usize);
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut regressions = Vec::new();
    let mut now_passing = Vec::new();

    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case ID {}",
            case.id
        );
        assert!(
            contexts.contains_key(&case.context),
            "{}: missing context",
            case.id
        );
        let is_pending = pending.contains(case.id.as_str());
        let failure = match &case.expected {
            Expected::Deferred { ambiguity_id } => {
                assert!(!is_pending, "{}: deferred cases cannot be PENDING", case.id);
                *deferred.entry(ambiguity_id).or_default() += 1;
                continue;
            }
            Expected::Exact { output } => {
                exact += 1;
                let actual = render_case(case, &contexts);
                (actual != *output)
                    .then(|| format!("{}: expected {output:?}, actual {actual:?}", case.id))
            }
            Expected::Invariant { .. } => {
                invariant += 1;
                invariant_failure(case, &contexts)
            }
        };
        let is_exact = matches!(case.expected, Expected::Exact { .. });
        match (failure, is_pending) {
            (None, false) if is_exact => exact_pass += 1,
            (None, false) => invariant_pass += 1,
            (Some(_), true) if is_exact => exact_pending += 1,
            (Some(_), true) => invariant_pending += 1,
            (Some(message), false) => regressions.push(message),
            (None, true) => now_passing.push(case.id.clone()),
        }
    }

    let stale: Vec<&&str> = pending.iter().filter(|id| !ids.contains(**id)).collect();
    assert!(
        stale.is_empty(),
        "PENDING names unknown case IDs: {stale:?}"
    );

    for group in &corpus.stability_groups {
        let outputs: BTreeSet<String> = group
            .case_ids
            .iter()
            .map(|id| {
                let case = corpus.cases.iter().find(|c| &c.id == id);
                render_case(case.expect("stability case exists"), &contexts)
            })
            .collect();
        if outputs.len() != 1 {
            regressions.push(format!(
                "{}: outputs differ across renames: {outputs:?}",
                group.id
            ));
        }
    }

    let deferred_total: usize = deferred.values().sum();
    println!(
        "voice template corpus: {} cases | exact {exact_pass} pass / {exact_pending} pending | \
         invariant {invariant_pass} pass / {invariant_pending} pending | {deferred_total} deferred \
         | {} stability groups",
        corpus.cases.len(),
        corpus.stability_groups.len(),
    );
    println!("deferred by ambiguity: {deferred:?}");

    assert!(
        now_passing.is_empty(),
        "PENDING cases now pass; remove them from PENDING: {now_passing:?}"
    );
    assert!(
        regressions.is_empty(),
        "{} corpus case(s) disagree with the engine:\n{}",
        regressions.len(),
        regressions.join("\n")
    );
    assert_eq!(exact_pass + exact_pending, exact, "exact counts reconcile");
    assert_eq!(
        invariant_pass + invariant_pending,
        invariant,
        "invariant counts reconcile"
    );
    assert_eq!(
        exact + invariant + deferred_total,
        corpus.cases.len(),
        "pass + pending + deferred reconcile with the corpus total"
    );
    assert_eq!(
        exact_pending + invariant_pending,
        PENDING.len(),
        "every PENDING ID counted"
    );
    assert!(
        MAX_NAME_LEN == 100,
        "corpus invariants assume a 100-character ceiling"
    );
}
