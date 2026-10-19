//! Template lint and six-scenario preview for voice-room names (V7b).
//!
//! Pure functions over the V5 parser and evaluator in [`crate::voice_naming`]:
//! no Discord, database or HTTP access. [`lint`] reports bounded, typed
//! findings (parse errors with a position, unknown tokens, empty renders and
//! conditions that never match); [`preview`] renders a template in the six
//! fixed [`Scenario`]s. Both are generic over [`ExtensionPolicy`], so the same
//! code serves the V5 passthrough policy and later condition/styling policies.
//!
//! ```
//! use two_bot_core::voice_naming::PassthroughExtensions;
//! use two_bot_core::voice_template_lint::{lint, preview, FindingKind};
//!
//! let report = lint("@@ownr@@'s room", &PassthroughExtensions);
//! assert_eq!(report.findings[0].kind, FindingKind::UnknownToken);
//! assert_eq!(preview("##", &PassthroughExtensions)[0].name, "#1");
//! ```

use std::fmt;

use crate::voice_naming::{
    parse, render, resolve_majority_game, Choice, Evaluation, Extension, ExtensionPolicy,
    GameOptions, NumberStyle, PartyInfo, RoomContext, Segment, Template, MAX_TEMPLATE_BYTES,
    MAX_TEMPLATE_DEPTH,
};

/// Most findings in one [`LintReport`]; further findings set `truncated`.
pub const MAX_FINDINGS: usize = 16;
/// Most input characters echoed by one [`Finding::excerpt`] (plus `…`).
pub const EXCERPT_CHARS: usize = 24;
/// Upper bound on [`Finding::message`] length in characters.
pub const MAX_MESSAGE_CHARS: usize = 120;
/// Seed shared by every scenario, so random picks never vary between runs.
pub const SCENARIO_SEED: u64 = 0x5CE7_A210_0000_0007;
/// Token names the V5 evaluator substitutes; any other `@@name@@` renders empty.
pub const KNOWN_TOKENS: [&str; 24] = [
    "owner",
    "creator",
    "original_creator",
    "num",
    "num_others",
    "num_live",
    "limit",
    "slots",
    "game_name",
    "stream_name",
    "num_playing",
    "party_size",
    "party_state",
    "party_details",
    "weekday",
    "month",
    "hour",
    "random_emoji",
    "nato",
    "daypart",
    "room_minutes",
    "room_tier",
    "game_minutes",
    "game_tier",
];

// Isolation parses of possibly unclosed constructs, and conditional blocks
// inspected, per lint. Each is bounded by MAX_TEMPLATE_BYTES of input.
const MAX_CONSTRUCT_CHECKS: usize = 64;
const MAX_CONDITIONALS: usize = 32;
// Private-use markers for probing a condition's truth through the policy.
const PROBE_YES: &str = "\u{E000}";
const PROBE_NO: &str = "\u{E001}";

/// The six fixed preview states named by the template assistant spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scenario {
    /// One member, nobody playing.
    SoloNoGame,
    /// Three members, all in the same game.
    ThreeInGame,
    /// Two members; the owner is live.
    OwnerStreaming,
    /// Four members in a game with rich-presence party data.
    GameWithParty,
    /// Four members under a limit of five.
    NearlyFull,
    /// Two members with the limit lowered to the headcount.
    Locked,
}

impl Scenario {
    /// Every scenario, in preview order.
    pub const ALL: [Scenario; 6] = [
        Scenario::SoloNoGame,
        Scenario::ThreeInGame,
        Scenario::OwnerStreaming,
        Scenario::GameWithParty,
        Scenario::NearlyFull,
        Scenario::Locked,
    ];

    /// Short English label for previews and findings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Scenario::SoloNoGame => "solo, no game",
            Scenario::ThreeInGame => "three in a game",
            Scenario::OwnerStreaming => "owner streaming",
            Scenario::GameWithParty => "game with party info",
            Scenario::NearlyFull => "nearly full",
            Scenario::Locked => "locked",
        }
    }

    /// The fixed room state for this scenario. Fields not set here keep their
    /// [`RoomContext::default`] values (temporary channel, UTC, no lists).
    #[must_use]
    pub fn context(self) -> RoomContext {
        match self {
            // Monday 2026-01-05 09:00 UTC.
            Scenario::SoloNoGame => room(1, "Avery", &[None], 1_767_603_600),
            // Saturday 2026-03-07 21:00 UTC.
            Scenario::ThreeInGame => room(2, "Blake", &[Some("Apex"); 3], 1_772_917_200),
            // Wednesday 2026-05-13 18:00 UTC.
            Scenario::OwnerStreaming => RoomContext {
                live_count: 1,
                stream_title: "Ranked grind".to_string(),
                ..room(3, "Casey", &[Some("Chess"), None], 1_778_695_200)
            },
            // Sunday 2026-07-12 15:00 UTC.
            Scenario::GameWithParty => RoomContext {
                parties: vec![PartyInfo {
                    size: 3,
                    max: Some(4),
                    state: "In Match".to_string(),
                    details: "Ranked".to_string(),
                }],
                ..room(4, "Devon", &[Some("Apex"); 4], 1_783_868_400)
            },
            // Friday 2026-09-18 23:00 UTC.
            Scenario::NearlyFull => RoomContext {
                user_limit: 5,
                ..room(
                    5,
                    "Emery",
                    &[Some("Chess"), Some("Chess"), Some("Chess"), None],
                    1_789_772_400,
                )
            },
            // Tuesday 2026-11-10 02:00 UTC; ownership passed on from the creator.
            Scenario::Locked => RoomContext {
                user_limit: 2,
                original_creator_name: "Avery".to_string(),
                ..room(6, "Finley", &[None, None], 1_794_276_000)
            },
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// The owner is the first member; the game title comes from the real resolver.
fn room(number: u32, owner: &str, activities: &[Option<&str>], timestamp: i64) -> RoomContext {
    let activities: Vec<Option<String>> = activities
        .iter()
        .map(|activity| activity.map(str::to_string))
        .collect();
    RoomContext {
        room_number: number,
        owner_name: owner.to_string(),
        original_creator_name: owner.to_string(),
        member_count: activities.len() as u32,
        owner_present: true,
        game_name: resolve_majority_game(
            &activities,
            activities.first().and_then(Option::as_deref),
            &GameOptions::default(),
        ),
        members_playing: activities.iter().flatten().count() as u32,
        timestamp,
        seed: SCENARIO_SEED,
        ..RoomContext::default()
    }
}

/// One scenario's final channel name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioRender {
    /// The scenario rendered.
    pub scenario: Scenario,
    /// Final name: trimmed, truncated and with the fallback applied.
    pub name: String,
    /// The template rendered empty, so `name` is the fallback name.
    pub used_fallback: bool,
}

/// Render `source` in all six scenarios, in [`Scenario::ALL`] order.
#[must_use]
pub fn preview<E: ExtensionPolicy>(source: &str, extensions: &E) -> [ScenarioRender; 6] {
    let template = parse(source);
    Scenario::ALL.map(|scenario| render_scenario(&template, scenario, extensions))
}

fn render_scenario<E: ExtensionPolicy>(
    template: &Template,
    scenario: Scenario,
    extensions: &E,
) -> ScenarioRender {
    let context = scenario.context();
    let used_fallback = Evaluation::new(&context, extensions)
        .evaluate(template)
        .trim()
        .is_empty();
    ScenarioRender {
        scenario,
        name: render(template, &context, extensions),
        used_fallback,
    }
}

/// A bracketed construct that can be left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Construct {
    /// `@@name@@`.
    Token,
    /// `<<singular/plural>>`.
    Plural,
    /// `[[a/b]]` or `[[list:name]]`.
    Choice,
    /// `__resting/in use__`.
    Resting,
    /// `{{cond ?? yes // no}}`.
    Conditional,
    /// `""mode:text""`.
    Styled,
}

/// Why part of a template renders as literal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The construct at the position never closes.
    Unclosed(Construct),
    /// Over [`MAX_TEMPLATE_BYTES`]: the whole template is literal.
    TooLong,
    /// Over [`MAX_TEMPLATE_DEPTH`] nested blocks: the whole template is literal.
    TooDeep,
}

/// What a [`Finding`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// Syntax the parser keeps as literal text.
    ParseError(ParseError),
    /// A `@@name@@` token outside [`KNOWN_TOKENS`].
    UnknownToken,
    /// The name is empty (so the fallback is used) in these scenarios.
    EmptyRender { scenarios: Vec<Scenario> },
    /// The policy evaluates this condition as false in every scenario.
    ConditionNeverMatches,
}

/// Errors make the name differ from what the template says; warnings flag
/// names that are valid but likely unintended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A location in the template source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Byte offset, always on a character boundary.
    pub byte: usize,
    /// Character offset, for display.
    pub char: usize,
}

/// One lint finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    /// Where the problem starts; `None` for whole-template findings.
    pub position: Option<Position>,
    /// At most [`EXCERPT_CHARS`] characters of source from `position`, plus
    /// `…` when cut. Empty when there is no position.
    pub excerpt: String,
}

impl Finding {
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self.kind {
            FindingKind::ParseError(_) | FindingKind::UnknownToken => Severity::Error,
            FindingKind::EmptyRender { .. } | FindingKind::ConditionNeverMatches => {
                Severity::Warning
            }
        }
    }

    /// Fixed English description, at most [`MAX_MESSAGE_CHARS`] characters.
    /// It never contains template text; see [`Finding::excerpt`].
    #[must_use]
    pub fn message(&self) -> String {
        match &self.kind {
            FindingKind::ParseError(ParseError::Unclosed(construct)) => {
                let syntax = match construct {
                    Construct::Token => "@@name@@ token",
                    Construct::Plural => "<<singular/plural>> block",
                    Construct::Choice => "[[a/b]] choice",
                    Construct::Resting => "__resting/in use__ block",
                    Construct::Conditional => "{{cond ?? yes // no}} block",
                    Construct::Styled => "\"\"mode:text\"\" style",
                };
                format!("{syntax} is unclosed or malformed; it renders as literal text")
            }
            FindingKind::ParseError(ParseError::TooLong) => {
                format!("template is over {MAX_TEMPLATE_BYTES} bytes; it renders as literal text")
            }
            FindingKind::ParseError(ParseError::TooDeep) => format!(
                "template nests over {MAX_TEMPLATE_DEPTH} blocks deep; it renders as literal text"
            ),
            FindingKind::UnknownToken => "unknown token; it renders as nothing".to_string(),
            FindingKind::EmptyRender { scenarios } => format!(
                "renders empty in {} of {} preview scenarios; the fallback name is used",
                scenarios.len(),
                Scenario::ALL.len()
            ),
            FindingKind::ConditionNeverMatches => {
                "condition is false in every preview scenario; its yes branch never shows"
                    .to_string()
            }
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity() {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        match self.position {
            Some(position) => write!(
                f,
                "{severity} at {}: {}: {}",
                position.char,
                self.message(),
                self.excerpt
            ),
            None => write!(f, "{severity}: {}", self.message()),
        }
    }
}

/// Lint result: errors in source order, then warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LintReport {
    /// At most [`MAX_FINDINGS`] findings.
    pub findings: Vec<Finding>,
    /// Findings were dropped or a scan bound was reached.
    pub truncated: bool,
}

impl LintReport {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity() == Severity::Error)
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && !self.truncated
    }
}

/// Lint `source` with `extensions` deciding conditions and styling.
///
/// Parse errors and unknown tokens follow the V5 parser exactly. Empty names
/// and never-matching conditions are judged over the six [`Scenario`]s only.
/// Under [`crate::voice_naming::PassthroughExtensions`] no condition has a
/// known truth value, so none is reported as never matching.
#[must_use]
pub fn lint<E: ExtensionPolicy>(source: &str, extensions: &E) -> LintReport {
    let template = parse(source);
    let mut linter = Linter {
        source,
        extensions,
        errors: Vec::new(),
        warnings: Vec::new(),
        construct_checks: 0,
        conditionals: 0,
        truncated: false,
    };
    if source.len() > MAX_TEMPLATE_BYTES {
        linter.push(FindingKind::ParseError(ParseError::TooLong), Some(0));
    } else if is_too_deep(source, &template) {
        linter.push(FindingKind::ParseError(ParseError::TooDeep), Some(0));
    } else {
        linter.walk(&template.0, Scope::root(source), 0);
    }

    let mut findings = std::mem::take(&mut linter.errors);
    findings.sort_by_key(|finding| finding.position.map(|position| position.byte));
    let empty: Vec<Scenario> = Scenario::ALL
        .into_iter()
        .filter(|scenario| render_scenario(&template, *scenario, extensions).used_fallback)
        .collect();
    if !empty.is_empty() {
        findings.push(Finding {
            kind: FindingKind::EmptyRender { scenarios: empty },
            position: None,
            excerpt: String::new(),
        });
    }
    findings.append(&mut linter.warnings);
    let mut truncated = linter.truncated;
    if findings.len() > MAX_FINDINGS {
        findings.truncate(MAX_FINDINGS);
        truncated = true;
    }
    LintReport {
        findings,
        truncated,
    }
}

// Depth overflow leaves the whole template literal. It needs more nested
// bodies than the limit, and only `""` styling cannot nest, so a literal parse
// with that many other openers is treated as too deep (a documented heuristic).
fn is_too_deep(source: &str, template: &Template) -> bool {
    let literal = matches!(template.0.as_slice(), [Segment::Text(text)] if text == source);
    literal && count_nesting_openers(source) >= MAX_TEMPLATE_DEPTH
}

fn count_nesting_openers(source: &str) -> usize {
    let bytes = source.as_bytes();
    let (mut count, mut i) = (0, 0);
    while i + 1 < bytes.len() {
        if bytes[i] == bytes[i + 1] && matches!(bytes[i], b'<' | b'[' | b'_' | b'{') {
            count += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    count
}

// Where segments came from. `src` is the exact string the parser read for
// them (local offsets index into it); `None` when that is unknown. Anchored
// scopes report every finding at `base`, the enclosing conditional.
#[derive(Clone, Copy)]
struct Scope<'s> {
    src: Option<&'s str>,
    base: usize,
    anchored: bool,
    tokens_only: bool,
}

impl<'s> Scope<'s> {
    fn root(source: &'s str) -> Self {
        Scope {
            src: Some(source),
            base: 0,
            anchored: false,
            tokens_only: false,
        }
    }

    fn position(self, local: usize) -> usize {
        if self.anchored {
            self.base
        } else {
            self.base + local
        }
    }

    // A separately parsed slice starting at `local` in this scope.
    fn child<'c>(self, src: Option<&'c str>, local: usize) -> Scope<'c> {
        Scope {
            src,
            base: self.position(local),
            anchored: self.anchored,
            tokens_only: self.tokens_only,
        }
    }
}

struct Linter<'a, E: ExtensionPolicy> {
    source: &'a str,
    extensions: &'a E,
    errors: Vec<Finding>,
    warnings: Vec<Finding>,
    construct_checks: usize,
    conditionals: usize,
    truncated: bool,
}

impl<E: ExtensionPolicy> Linter<'_, E> {
    fn push(&mut self, kind: FindingKind, byte: Option<usize>) {
        // Positions come from parser spans; clamp anyway so lint never panics.
        let byte = byte.map(|byte| {
            let mut byte = byte.min(self.source.len());
            while !self.source.is_char_boundary(byte) {
                byte -= 1;
            }
            byte
        });
        let finding = Finding {
            position: byte.map(|byte| Position {
                byte,
                char: self.source[..byte].chars().count(),
            }),
            excerpt: byte.map_or_else(String::new, |byte| excerpt(self.source, byte)),
            kind,
        };
        let list = match finding.severity() {
            Severity::Error => &mut self.errors,
            Severity::Warning => &mut self.warnings,
        };
        if list.len() < MAX_FINDINGS {
            list.push(finding);
        } else {
            self.truncated = true;
        }
    }

    // Visit `segments` starting at `start` within `scope`; returns the end.
    fn walk(&mut self, segments: &[Segment], scope: Scope<'_>, start: usize) -> usize {
        let mut at = start;
        for segment in segments {
            match segment {
                Segment::Text(text) if !scope.tokens_only => self.scan_text(text, scope, at),
                Segment::Token(name) if !KNOWN_TOKENS.contains(&name.as_str()) => {
                    self.push(FindingKind::UnknownToken, Some(scope.position(at)));
                }
                Segment::Plural {
                    singular,
                    plural,
                    has_separator,
                    ..
                } => {
                    let end = self.walk(&singular.0, scope, at + 2);
                    if *has_separator {
                        self.walk(&plural.0, scope, end + 1);
                    }
                }
                Segment::Choice(Choice::Options(options)) => {
                    let mut next = at + 2;
                    for option in options {
                        next = self.walk(&option.0, scope, next) + 1;
                    }
                }
                Segment::Resting {
                    resting, in_use, ..
                } => {
                    let end = self.walk(&resting.0, scope, at + 2);
                    if let Some(in_use) = in_use {
                        self.walk(&in_use.0, scope, end + 1);
                    }
                }
                Segment::Extension(Extension::Conditional { source, body })
                    if !scope.tokens_only =>
                {
                    self.conditional(source, body, scope, at);
                }
                Segment::Extension(Extension::Styled {
                    source,
                    modes,
                    body,
                }) => {
                    // The parser reads the body as its own slice.
                    let body_start = 2 + modes.len() + 1;
                    let body_src = source.get(body_start..source.len().saturating_sub(2));
                    self.walk(&body.0, scope.child(body_src, at + body_start), 0);
                }
                _ => {}
            }
            at = segment_end(segment, scope.src, at);
        }
        at
    }

    // Literal text holds every construct the parser could not close. A
    // construct parses the same at any depth, so re-parsing its suffix alone
    // tells a root failure from a valid block swallowed by an outer failure.
    fn scan_text(&mut self, text: &str, scope: Scope<'_>, local: usize) {
        let mut i = 0;
        while let Some((offset, construct)) = next_opener(&text[i..]) {
            let p = i + offset;
            i = p + 2;
            let suffix = scope
                .src
                .and_then(|src| src.get(local + p..))
                .filter(|suffix| suffix.get(..2) == text.get(p..p + 2))
                .unwrap_or(&text[p..]);
            let closes = if construct == Construct::Token {
                token_closes(suffix)
            } else if self.construct_checks < MAX_CONSTRUCT_CHECKS {
                self.construct_checks += 1;
                !matches!(parse(suffix).0.first(), Some(Segment::Text(_)) | None)
            } else {
                self.truncated = true;
                return;
            };
            if !closes {
                let kind = FindingKind::ParseError(ParseError::Unclosed(construct));
                self.push(kind, Some(scope.position(local + p)));
            }
        }
    }

    fn conditional(&mut self, source: &str, body: &Template, scope: Scope<'_>, at: usize) {
        if self.conditionals == MAX_CONDITIONALS {
            self.truncated = true;
            return;
        }
        self.conditionals += 1;
        let inner = source.get(2..source.len().saturating_sub(2)).unwrap_or("");
        let (condition, branches) = split_conditional(inner);
        let parsed: Vec<Template> = branches.iter().map(|(_, branch)| parse(branch)).collect();
        if merge_branches(&parsed) != *body {
            // The separators found here differ from the parser's (possible
            // around `""` styling): anchor nested findings at the block.
            let anchored = Scope {
                src: None,
                base: scope.position(at),
                anchored: true,
                tokens_only: false,
            };
            self.walk(&body.0, anchored, 0);
            return;
        }
        if let Some(condition) = condition {
            let tokens = Scope {
                tokens_only: true,
                ..scope.child(Some(condition), at + 2)
            };
            self.walk(&parse(condition).0, tokens, 0);
            if self.never_matches(condition) {
                let kind = FindingKind::ConditionNeverMatches;
                self.push(kind, Some(scope.position(at)));
            }
        }
        for (&(offset, branch), template) in branches.iter().zip(&parsed) {
            self.walk(&template.0, scope.child(Some(branch), at + 2 + offset), 0);
        }
    }

    // Ask the policy for the condition's truth with marker branches. Output
    // with only the no marker is false; anything else is true or unknown.
    fn never_matches(&self, condition: &str) -> bool {
        let probe = ["{{", condition, "??", PROBE_YES, "//", PROBE_NO, "}}"].concat();
        let template = parse(&probe);
        let single = matches!(
            template.0.as_slice(),
            [Segment::Extension(Extension::Conditional { .. })]
        );
        if !single || split_conditional(&probe[2..probe.len() - 2]).0 != Some(condition) {
            return false;
        }
        Scenario::ALL.iter().all(|scenario| {
            let context = scenario.context();
            let out = Evaluation::new(&context, self.extensions).evaluate(&template);
            out.contains(PROBE_NO) && !out.contains(PROBE_YES)
        })
    }
}

// The first opener in `text` and the construct it starts. All openers are
// two equal ASCII bytes, so every match is on a character boundary.
fn next_opener(text: &str) -> Option<(usize, Construct)> {
    text.as_bytes()
        .windows(2)
        .enumerate()
        .find_map(|(i, pair)| {
            let construct = match pair {
                b"@@" => Construct::Token,
                b"<<" => Construct::Plural,
                b"[[" => Construct::Choice,
                b"__" => Construct::Resting,
                b"{{" => Construct::Conditional,
                b"\"\"" => Construct::Styled,
                _ => return None,
            };
            Some((i, construct))
        })
}

fn token_closes(suffix: &str) -> bool {
    let name = suffix[2..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(suffix.len() - 2);
    name > 0 && suffix[2 + name..].starts_with("@@")
}

// Source length of one parsed segment starting at `start` in `src`.
fn segment_end(segment: &Segment, src: Option<&str>, start: usize) -> usize {
    match segment {
        Segment::Text(text) => start + text.len(),
        Segment::Number(NumberStyle::Bare { width }) => start + width + 1,
        Segment::Number(_) => start + 2,
        Segment::Token(name) => start + name.len() + 4,
        Segment::Plural {
            singular,
            plural,
            has_separator,
            ..
        } => {
            let mut end = template_end(singular, src, start + 2);
            if *has_separator {
                end = template_end(plural, src, end + 1);
            }
            end + 2
        }
        Segment::Choice(Choice::Options(options)) => {
            let mut end = start + 2;
            for (index, option) in options.iter().enumerate() {
                end = template_end(option, src, end + usize::from(index > 0));
            }
            end + 2
        }
        // `[[list:` + raw name + `]]`; the stored name is trimmed.
        Segment::Choice(Choice::NamedList(_)) => {
            start
                + src
                    .and_then(|src| src.get(start + 7..))
                    .and_then(|rest| rest.find("]]"))
                    .map_or(0, |end| 7 + end + 2)
        }
        Segment::Resting { source, .. }
        | Segment::Extension(Extension::Conditional { source, .. })
        | Segment::Extension(Extension::Styled { source, .. }) => start + source.len(),
    }
}

fn template_end(template: &Template, src: Option<&str>, start: usize) -> usize {
    template
        .0
        .iter()
        .fold(start, |at, segment| segment_end(segment, src, at))
}

// Split a conditional's inner text at its first top-level `??`, then the
// first top-level `//` after it. Returns the condition and each branch with
// its offset in `inner`.
fn split_conditional(inner: &str) -> (Option<&str>, Vec<(usize, &str)>) {
    let Some(question) = top_level_find(inner, "??") else {
        return (None, vec![(0, inner)]);
    };
    let yes_start = question + 2;
    let rest = &inner[yes_start..];
    let branches = match top_level_find(rest, "//") {
        Some(slash) => vec![
            (yes_start, &rest[..slash]),
            (yes_start + slash + 2, &rest[slash + 2..]),
        ],
        None => vec![(yes_start, rest)],
    };
    (Some(&inner[..question]), branches)
}

// A separator never starts or ends a non-text segment, so it lies wholly
// inside one top-level literal.
fn top_level_find(input: &str, separator: &str) -> Option<usize> {
    let mut at = 0;
    for segment in &parse(input).0 {
        if let Segment::Text(text) = segment {
            if let Some(found) = text.find(separator) {
                return Some(at + found);
            }
        }
        at = segment_end(segment, Some(input), at);
    }
    None
}

// Concatenate branch ASTs the way the parser builds a conditional's body.
fn merge_branches(branches: &[Template]) -> Template {
    let mut body: Vec<Segment> = Vec::new();
    for segment in branches.iter().flat_map(|branch| &branch.0) {
        match (body.last_mut(), segment) {
            (Some(Segment::Text(previous)), Segment::Text(next)) => previous.push_str(next),
            _ => body.push(segment.clone()),
        }
    }
    Template(body)
}

fn excerpt(source: &str, byte: usize) -> String {
    let mut chars = source[byte..].chars();
    let mut out: String = chars.by_ref().take(EXCERPT_CHARS).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}
