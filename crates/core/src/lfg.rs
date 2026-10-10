//! LFG (looking-for-group) raid signup domain: role-spec parsing, post
//! validation, signup adjudication, message rendering, and select-menu data.
//!
//! Slice TOG-10084 of TOG-9809. Ports the DB/Discord-free core of the legacy
//! announcements LFG feature as framework-free data plus pure functions, same
//! style as `leveling.rs`/`moderation.rs`. The sqlx store lives in
//! `lfg_store.rs` (behind the `db` feature); the interaction-router handlers
//! and REST side effects (post/edit the signup message, refresh on
//! signup/leave/close) wire in a follow-up once the S4 router (TOG-10075) and
//! REST executor (TOG-10076) slices land — this module exposes outcome enums
//! and reply-text helpers so that wiring is mechanical. No private dispatcher
//! or HTTP client lives here.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - shapes: `src/announcements/discord.ts` (`announcementCommandData`
//!   `lfg`/`lfg-close`, both `ManageEvents` at builder AND runtime;
//!   `LFG_PREFIX = 'two:lfg:'` select handler with the `__leave__` value).
//! - domain: `src/announcements/service.ts` (`parseRoleSpec`,
//!   `normalizeRoles`, `normalizeFutureTimestamp`, title bounds,
//!   `renderLfg`, `lfgNonce`).
//! - store semantics: `src/announcements/store.ts` (`signupLfg` outcome order
//!   missing → closed → missing-role → joined(same) → full → moved/joined,
//!   `pg_advisory_xact_lock` fencing, `closeLfg` open-only update).
//! - DDL: `migrations/0024_announcements_feeds.sql` (`lfg_posts`,
//!   `lfg_roles`, `lfg_signups`; table/column names preserved verbatim).
//!
//! Deliberately out of scope: RSVP/feed tables in the same legacy migration
//! file (TOG-10083 / TOG-10085 own those), the `announcements_audit_log`
//! writes (the shared audit table lands with the S6 store port, TOG-9811 —
//! the outcome enums here carry everything the audit rows need), and the
//! temp-voice runtime (matrix §9 drop 6).

use super::commands::PERM_MANAGE_EVENTS;

/// Legacy `MAX_TITLE_CHARS`.
pub const MAX_TITLE_CHARS: usize = 100;
/// Legacy `MAX_LFG_ROLES`.
pub const MAX_LFG_ROLES: usize = 20;
pub const MAX_ROLE_KEY_CHARS: usize = 32;
pub const MAX_ROLE_LABEL_CHARS: usize = 80;
/// Longest canonical comma-separated role specification accepted by `/lfg`.
pub const MAX_ROLE_SPEC_CHARS: usize =
    MAX_LFG_ROLES * (MAX_ROLE_KEY_CHARS + 1 + MAX_ROLE_LABEL_CHARS + 1 + 2) + (MAX_LFG_ROLES - 1);
const MAX_ROLE_KEY_ERROR_CHARS: usize = 128;
/// Legacy role slot bounds (1–99).
pub const MAX_ROLE_SLOTS: u8 = 99;
/// Legacy `LFG_PREFIX` — select `custom_id` is `two:lfg:<post id>`.
pub const LFG_SELECT_PREFIX: &str = "two:lfg:";
/// Legacy leave value in the signup select.
pub const LFG_LEAVE_VALUE: &str = "__leave__";
/// Legacy `renderLfg` content cap (`content.slice(0, 2000)`).
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Legacy select option label cap (`.slice(0, 100)`).
pub const MAX_OPTION_LABEL_CHARS: usize = 100;

/// One parsed `key:label:slots` role entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfgRoleSpec {
    /// Lowercased key (`tank`); matches `^[a-z0-9_-]{1,32}$`.
    pub key: String,
    /// Display label (`Tank`); 1–80 chars.
    pub label: String,
    /// Slot count; 1–99.
    pub slots: u8,
}

/// Role-spec refusal (legacy `normalizeRoles` / `parseRoleSpec` throws).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoleSpecError {
    /// A comma-separated entry is not exactly `key:label:slots`.
    #[error("roles must be key:label:slots entries separated by commas.")]
    BadShape,
    /// Key fails `^[a-z0-9_-]{{1,32}}$` after trim+lowercase; carries a bounded raw prefix.
    #[error("Invalid LFG role key \"{0}\".")]
    BadKey(String),
    /// The raw specification exceeds the published input bound.
    #[error("LFG role specification is too long.")]
    TooLong,
    /// Normalized key equals the reserved leave-action value (`__leave__`);
    /// carries the normalized key. Legacy `normalizeRoles` throws
    /// `LFG role key "__leave__" is reserved for leaving the group.`
    #[error("LFG role key \"{0}\" is reserved for leaving the group.")]
    ReservedKey(String),
    /// Label empty or longer than 80 chars.
    #[error("LFG role labels must be 1-80 characters.")]
    BadLabel,
    /// Slots not an integer in 1–99.
    #[error("LFG role slots must be integers from 1-99.")]
    BadSlots,
    /// Not 1–20 roles.
    #[error("LFG needs 1-20 roles.")]
    BadCount,
    /// Normalized key seen twice; carries the normalized key.
    #[error("Duplicate LFG role key \"{0}\".")]
    DuplicateKey(String),
}

/// ECMAScript WhiteSpace + LineTerminator for legacy `trim()` / `Number()`.
/// Unlike Rust whitespace, this includes BOM and excludes U+0085 (NEL).
fn trim_ecmascript(value: &str) -> &str {
    value.trim_matches(|ch| {
        matches!(
            ch,
            '\u{0009}'..='\u{000d}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// Parse a `tank:Tank:2,healer:Healer:2,dps:DPS:6` role spec (legacy
/// `parseRoleSpec` + `normalizeRoles`).
pub fn parse_role_spec(spec: &str) -> Result<Vec<LfgRoleSpec>, RoleSpecError> {
    if spec.encode_utf16().take(MAX_ROLE_SPEC_CHARS + 1).count() > MAX_ROLE_SPEC_CHARS {
        return Err(RoleSpecError::TooLong);
    }
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.is_empty() || parts.len() > MAX_LFG_ROLES {
        return Err(RoleSpecError::BadCount);
    }
    let mut seen = std::collections::HashSet::new();
    let mut roles = Vec::with_capacity(parts.len());
    for part in parts {
        let fields: Vec<&str> = part.split(':').collect();
        if fields.len() != 3 {
            return Err(RoleSpecError::BadShape);
        }
        let raw_key = fields[0];
        let key = trim_ecmascript(raw_key).to_lowercase();
        if !valid_role_key(&key) {
            return Err(RoleSpecError::BadKey(
                raw_key.chars().take(MAX_ROLE_KEY_ERROR_CHARS).collect(),
            ));
        }
        if key == LFG_LEAVE_VALUE {
            return Err(RoleSpecError::ReservedKey(key));
        }
        let label = trim_ecmascript(fields[1]);
        if label.is_empty() || label.encode_utf16().count() > MAX_ROLE_LABEL_CHARS {
            return Err(RoleSpecError::BadLabel);
        }
        let slots = parse_slots(fields[2]).ok_or(RoleSpecError::BadSlots)?;
        if !seen.insert(key.clone()) {
            return Err(RoleSpecError::DuplicateKey(key));
        }
        roles.push(LfgRoleSpec {
            key,
            label: label.to_owned(),
            slots,
        });
    }
    Ok(roles)
}

/// Legacy key pattern `/^[a-z0-9_-]{1,32}$/` on the normalized key.
#[must_use]
pub fn valid_role_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_ROLE_KEY_CHARS
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn parse_radix_number(digits: &str, radix: u32) -> Option<f64> {
    if digits.starts_with('+') || digits.starts_with('-') {
        return None;
    }
    Some(u64::from_str_radix(digits, radix).ok()? as f64)
}

/// Legacy `Number(fields[2])`: decimal/exponent and unsigned radix forms,
/// followed by the integer/range check. Empty and non-finite values fail.
fn parse_slots(raw: &str) -> Option<u8> {
    let trimmed = trim_ecmascript(raw);
    if trimmed.is_empty() {
        return None;
    }
    let value = if let Some(digits) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        parse_radix_number(digits, 16)?
    } else if let Some(digits) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        parse_radix_number(digits, 2)?
    } else if let Some(digits) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        parse_radix_number(digits, 8)?
    } else {
        trimmed.parse::<f64>().ok()?
    };
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    if value < 1.0 || value > f64::from(MAX_ROLE_SLOTS) {
        return None;
    }
    Some(value as u8)
}

/// Invalid `title` (legacy `createLfg` title check).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TitleError {
    #[error("LFG title must be 1-100 characters.")]
    BadTitle,
}

/// Trim and enforce the legacy 1–100 UTF-16 code-unit title limit.
pub fn validate_title(title: &str) -> Result<String, TitleError> {
    let trimmed = trim_ecmascript(title);
    if trimmed.is_empty() || trimmed.encode_utf16().count() > MAX_TITLE_CHARS {
        return Err(TitleError::BadTitle);
    }
    Ok(trimmed.to_owned())
}

/// Invalid `starts-at` (legacy `normalizeFutureTimestamp` throws).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartsAtError {
    #[error("starts-at must be an ISO-8601 timestamp.")]
    NotIso8601,
    #[error("starts-at must be in the future.")]
    NotFuture,
}

/// Validate `starts-at` and normalize to a UTC `toISOString()`-shaped string
/// (`YYYY-MM-DDTHH:mm:ss.sssZ`).
///
/// Legacy runs `Date.parse`, which also accepts non-ISO strings ("September
/// 11, 2026"); this port only accepts RFC-3339/ISO-8601 per the command
/// option's documented "ISO-8601 start time" contract, and refuses anything
/// else with the same legacy message. `now_unix_ms` is the caller's clock
/// (unix millis), mirroring legacy's injectable `now` parameter.
pub fn normalize_starts_at(value: &str, now_unix_ms: i64) -> Result<String, StartsAtError> {
    use time::format_description::well_known::Rfc3339;
    let parsed = time::OffsetDateTime::parse(value.trim(), &Rfc3339)
        .map_err(|_| StartsAtError::NotIso8601)?;
    let ms = parsed.unix_timestamp() * 1000 + i64::from(parsed.millisecond());
    if ms <= now_unix_ms {
        return Err(StartsAtError::NotFuture);
    }
    Ok(iso_millis_utc(parsed))
}

/// Format an instant as legacy `Date.toISOString()` does
/// (`YYYY-MM-DDTHH:mm:ss.sssZ`, always UTC, always millis).
#[must_use]
pub fn iso_millis_utc(dt: time::OffsetDateTime) -> String {
    let utc = dt.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.millisecond(),
    )
}

/// Post status (legacy `lfg_posts.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfgStatus {
    Open,
    Closed,
}

impl LfgStatus {
    /// Legacy stored string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    /// Legacy rendered word (`Open`/`Closed`).
    #[must_use]
    pub fn display(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
        }
    }

    /// Parse the stored string; `None` for anything else.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

/// One `lfg_posts` row (plain data; column names match the DDL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfgPost {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub title: String,
    /// Normalized UTC ISO-8601 (`normalize_starts_at` output).
    pub starts_at: String,
    pub status: LfgStatus,
    pub created_by: String,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// One `lfg_roles` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfgRole {
    pub lfg_id: String,
    pub role_key: String,
    pub label: String,
    pub slots: u8,
    pub position: usize,
}

/// One `lfg_signups` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfgSignup {
    pub lfg_id: String,
    pub user_id: String,
    pub role_key: String,
    pub joined_at: String,
}

/// Build [`LfgRole`] rows from a validated spec (legacy `createLfg` mapping).
#[must_use]
pub fn spec_roles(lfg_id: &str, spec: &[LfgRoleSpec]) -> Vec<LfgRole> {
    spec.iter()
        .enumerate()
        .map(|(position, role)| LfgRole {
            lfg_id: lfg_id.to_owned(),
            role_key: role.key.clone(),
            label: role.label.clone(),
            slots: role.slots,
            position,
        })
        .collect()
}

/// Signup outcome (legacy `signupLfg` return strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignupOutcome {
    Joined,
    Moved,
    Full,
    Closed,
    Missing,
}

impl SignupOutcome {
    /// Legacy wire string (also the ephemeral reply word).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Joined => "joined",
            Self::Moved => "moved",
            Self::Full => "full",
            Self::Closed => "closed",
            Self::Missing => "missing",
        }
    }
}

/// Pure capacity adjudication mirroring the legacy `signupLfg` transaction
/// order: missing post → closed post → unknown role → same-role re-signup →
/// capacity → moved/joined. The store reads the facts (`used` excludes the
/// requesting user, like the legacy `user_id <> ?` count) and calls this, so
/// the decision order is unit-testable without a database.
#[must_use]
pub fn adjudicate_signup(
    post_exists: bool,
    status: LfgStatus,
    role: Option<u8>,
    current_role_key: Option<&str>,
    requested_role_key: &str,
    used_slots: u64,
) -> SignupOutcome {
    if !post_exists {
        return SignupOutcome::Missing;
    }
    if status != LfgStatus::Open {
        return SignupOutcome::Closed;
    }
    let Some(slots) = role else {
        return SignupOutcome::Missing;
    };
    if current_role_key == Some(requested_role_key) {
        return SignupOutcome::Joined;
    }
    if used_slots >= u64::from(slots) {
        return SignupOutcome::Full;
    }
    if current_role_key.is_some() {
        SignupOutcome::Moved
    } else {
        SignupOutcome::Joined
    }
}

/// Count signups holding a role (legacy `renderLfg` per-role filter).
#[must_use]
pub fn role_fill(signups: &[LfgSignup], role_key: &str) -> usize {
    signups.iter().filter(|s| s.role_key == role_key).count()
}

/// Cap UTF-16 length like legacy without splitting a Unicode scalar.
fn truncate_utf16(value: &str, limit: usize) -> String {
    crate::message_safety::truncate(value, limit)
}

/// Post body (legacy `renderLfg` content): title, state, Discord timestamp,
/// and one `**label** n/slots[ — mentions]` line per role, capped at 2000
/// UTF-16 code units.
#[must_use]
pub fn lfg_content(post: &LfgPost, roles: &[LfgRole], signups: &[LfgSignup]) -> String {
    use time::format_description::well_known::Rfc3339;
    let unix_secs = time::OffsetDateTime::parse(&post.starts_at, &Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .unwrap_or(0);
    let mut lines = Vec::with_capacity(roles.len() + 2);
    lines.push(format!("**{}** — {}", post.title, post.status.display()));
    lines.push(format!("Starts <t:{unix_secs}:F>"));
    for role in roles {
        let members: Vec<String> = signups
            .iter()
            .filter(|s| s.role_key == role.role_key)
            .map(|s| format!("<@{}>", s.user_id))
            .collect();
        let fill = members.len();
        let mut line = format!("**{}** {fill}/{}", role.label, role.slots);
        if !members.is_empty() {
            line.push_str(&format!(" — {}", members.join(", ")));
        }
        lines.push(line);
    }
    let content = lines.join("\n");
    crate::message_safety::content(&content)
}

/// One signup-select option (plain data; the adapter maps this to the
/// Discord string-select shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfgSelectOption {
    pub label: String,
    pub value: String,
}

/// Select options for an open post: one per role (`label (n/slots)`, capped
/// at 100 chars like legacy) plus the leave entry. Closed posts render no
/// components (legacy `components = []`), so this returns empty.
#[must_use]
pub fn lfg_select_options(
    status: LfgStatus,
    roles: &[LfgRole],
    signups: &[LfgSignup],
) -> Vec<LfgSelectOption> {
    if status != LfgStatus::Open {
        return Vec::new();
    }
    let mut options: Vec<LfgSelectOption> = roles
        .iter()
        .map(|role| {
            let label = truncate_utf16(
                &format!(
                    "{} ({}/{})",
                    role.label,
                    role_fill(signups, &role.role_key),
                    role.slots
                ),
                MAX_OPTION_LABEL_CHARS,
            );
            LfgSelectOption {
                label,
                value: role.role_key.clone(),
            }
        })
        .collect();
    options.push(LfgSelectOption {
        label: "Leave this group".to_owned(),
        value: LFG_LEAVE_VALUE.to_owned(),
    });
    options
}

/// Select `custom_id` for a post (legacy `` `two:lfg:${post.id}` ``).
#[must_use]
pub fn lfg_custom_id(post_id: &str) -> String {
    format!("{LFG_SELECT_PREFIX}{post_id}")
}

/// What a signup-select interaction asks for (legacy select handler).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfgSelectAction {
    Signup { post_id: String, role_key: String },
    Leave { post_id: String },
}

/// Parse a select interaction (`custom_id` + chosen value). `None` for
/// foreign prefixes. An empty/unknown role value parses as a signup and the
/// store answers `missing`, exactly like legacy (`role ?? ''` → `signupLfg`
/// → unknown role → `'missing'`).
#[must_use]
pub fn parse_lfg_select(custom_id: &str, value: &str) -> Option<LfgSelectAction> {
    let post_id = custom_id.strip_prefix(LFG_SELECT_PREFIX)?;
    if value == LFG_LEAVE_VALUE {
        Some(LfgSelectAction::Leave {
            post_id: post_id.to_owned(),
        })
    } else {
        Some(LfgSelectAction::Signup {
            post_id: post_id.to_owned(),
            role_key: value.to_owned(),
        })
    }
}

/// Missing `ManageEvents` at runtime (legacy `/lfg` + `/lfg-close` handler
/// throw; the builder-side gate already lives in `feature_commands.rs`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LfgPermissionError {
    #[error("Manage Events permission is required.")]
    ManageEventsRequired,
}

/// Runtime permission gate (legacy `interaction.memberPermissions?.has(...)`).
pub fn require_manage_events(permissions: u64) -> Result<(), LfgPermissionError> {
    if permissions & PERM_MANAGE_EVENTS == 0 {
        return Err(LfgPermissionError::ManageEventsRequired);
    }
    Ok(())
}

/// Ephemeral reply after `/lfg` (legacy `` `LFG posted: \`${post.id}\`.` ``).
#[must_use]
pub fn created_reply(post_id: &str) -> String {
    format!("LFG posted: `{post_id}`.")
}

/// Ephemeral reply after a select signup (legacy `` `LFG ${outcome}.` ``).
#[must_use]
pub fn signup_reply(outcome: SignupOutcome) -> String {
    format!("LFG {}.", outcome.as_str())
}

/// Ephemeral reply after a select leave (legacy `'left'` /
/// `'were not signed up'`).
#[must_use]
pub fn leave_reply(removed: bool) -> String {
    if removed {
        "LFG left.".to_owned()
    } else {
        "LFG were not signed up.".to_owned()
    }
}

/// Ephemeral reply after `/lfg-close` (legacy `'LFG closed.'` /
/// `'LFG was already closed or missing.'`).
#[must_use]
pub fn close_reply(closed: bool) -> String {
    if closed {
        "LFG closed.".to_owned()
    } else {
        "LFG was already closed or missing.".to_owned()
    }
}

/// Stable post-message nonce (legacy `lfgNonce`: `sha256('lfg\0' + id)`,
/// hex, first 24 chars). Lets an ambiguous post (accepted-but-unknown)
/// recover the accepted message instead of double-posting.
#[must_use]
pub fn lfg_nonce(post_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"lfg\0");
    hasher.update(post_id.as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..24]
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_lfg_parsers_accept_arbitrary_unicode_without_panicking(
            text in proptest::collection::vec(any::<char>(), 0..256)
                .prop_map(|chars| chars.into_iter().collect::<String>()),
            now in any::<i64>(),
        ) {
            if let Ok(roles) = parse_role_spec(&text) {
                let wire = roles.iter().map(|r| format!("{}:{}:{}", r.key, r.label, r.slots))
                    .collect::<Vec<_>>().join(",");
                prop_assert_eq!(parse_role_spec(&wire), Ok(roles));
            }
            if let Ok(title) = validate_title(&text) {
                prop_assert_eq!(validate_title(&title), Ok(title));
            }
            if let Ok(instant) = normalize_starts_at(&text, now) {
                prop_assert_eq!(normalize_starts_at(&instant, now), Ok(instant));
            }
            let _ = parse_lfg_select(&text, &text);
        }

        #[test]
        fn property_role_specs_round_trip_normalized_values(
            entries in proptest::collection::vec(("[A-Za-z][A-Za-z0-9 _-]{0,79}", 1u8..=99), 1..=20),
        ) {
            let wire = entries.iter().enumerate()
                .map(|(i, (label, slots))| format!(" ROLE{i} : {label} : {slots} "))
                .collect::<Vec<_>>().join(",");
            let parsed = parse_role_spec(&wire).unwrap();
            let expected = entries.iter().enumerate().map(|(i, (label, slots))| LfgRoleSpec {
                key: format!("role{i}"), label: label.trim().to_owned(), slots: *slots,
            }).collect::<Vec<_>>();
            prop_assert_eq!(&parsed, &expected);
            let canonical = parsed.iter().map(|r| format!("{}:{}:{}", r.key, r.label, r.slots))
                .collect::<Vec<_>>().join(",");
            prop_assert_eq!(parse_role_spec(&canonical), Ok(parsed));
        }

        #[test]
        fn property_lfg_bounds_match_legacy_utf16_and_slot_limits(
            count in 0usize..=22,
            slots in -2i32..=102,
            chars in proptest::collection::vec(prop::sample::select(vec!['a', 'é', '😀', '\u{0085}']), 0..=110),
        ) {
            let spec = (0..count).map(|i| format!("r{i}:Role:{slots}"))
                .collect::<Vec<_>>().join(",");
            prop_assert_eq!(parse_role_spec(&spec).is_ok(), (1..=20).contains(&count) && (1..=99).contains(&slots));
            let title: String = chars.into_iter().collect();
            let expected = (1..=100).contains(&title.encode_utf16().count());
            prop_assert_eq!(validate_title(&format!("\u{feff}{title}\u{feff}")).is_ok(), expected);
            let label_spec = format!("role:{title}:1");
            prop_assert_eq!(parse_role_spec(&label_spec).is_ok(), (1..=80).contains(&title.encode_utf16().count()));
        }

        #[test]
        fn property_starts_at_normalizes_offsets_and_enforces_future_boundary(
            seconds in 946_684_800i64..4_102_444_800,
            millis in 0u16..1000,
            offset_minutes in -720i32..=840,
        ) {
            use time::format_description::well_known::Rfc3339;
            let instant = time::OffsetDateTime::from_unix_timestamp(seconds).unwrap()
                + time::Duration::milliseconds(i64::from(millis));
            let local = instant.to_offset(time::UtcOffset::from_whole_seconds(offset_minutes * 60).unwrap());
            let input = local.format(&Rfc3339).unwrap();
            let now = seconds * 1000 + i64::from(millis);
            let normalized = normalize_starts_at(&input, now - 1).unwrap();
            prop_assert_eq!(&normalized, &iso_millis_utc(instant));
            prop_assert_eq!(normalize_starts_at(&normalized, now - 1), Ok(normalized));
            prop_assert_eq!(normalize_starts_at(&input, now), Err(StartsAtError::NotFuture));
            prop_assert_eq!(normalize_starts_at(&input, now + 1), Err(StartsAtError::NotFuture));
        }
    }

    #[test]
    fn role_spec_parses_legacy_example() {
        let roles = parse_role_spec("tank:Tank:2,healer:Healer:2,dps:DPS:6").expect("parses");
        assert_eq!(
            roles,
            vec![
                LfgRoleSpec {
                    key: "tank".to_owned(),
                    label: "Tank".to_owned(),
                    slots: 2
                },
                LfgRoleSpec {
                    key: "healer".to_owned(),
                    label: "Healer".to_owned(),
                    slots: 2
                },
                LfgRoleSpec {
                    key: "dps".to_owned(),
                    label: "DPS".to_owned(),
                    slots: 6
                },
            ]
        );
    }

    #[test]
    fn role_spec_normalizes_key_and_trims() {
        let roles = parse_role_spec("  Tank : Main Tank : 4 ").expect("parses");
        assert_eq!(roles[0].key, "tank");
        assert_eq!(roles[0].label, "Main Tank");
        assert_eq!(roles[0].slots, 4);
    }

    #[test]
    fn ecmascript_whitespace_is_trimmed_from_roles_slots_and_titles() {
        // ECMAScript WhiteSpace + LineTerminator, including BOM (not Rust whitespace).
        for ch in [
            '\u{0009}', '\u{000a}', '\u{000b}', '\u{000c}', '\u{000d}', '\u{0020}', '\u{00a0}',
            '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
            '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}',
            '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}',
        ] {
            let roles = parse_role_spec(&format!("{ch}Tank{ch}:{ch}Main Tank{ch}:{ch}2{ch}"))
                .expect("legacy trims this character");
            assert_eq!(roles[0].key, "tank", "{ch:?}");
            assert_eq!(roles[0].label, "Main Tank", "{ch:?}");
            assert_eq!(roles[0].slots, 2, "{ch:?}");
            assert_eq!(
                validate_title(&format!("{ch}Friday raid{ch}")),
                Ok("Friday raid".to_owned()),
                "{ch:?}"
            );
            assert_eq!(validate_title(&ch.to_string()), Err(TitleError::BadTitle));
            assert_eq!(
                parse_role_spec(&format!("tank:{ch}:2")),
                Err(RoleSpecError::BadLabel),
                "{ch:?}"
            );
            assert_eq!(
                parse_role_spec(&format!("tank:Tank:{ch}")),
                Err(RoleSpecError::BadSlots),
                "{ch:?}"
            );
        }
        assert_eq!(
            parse_role_spec("\u{feff}Tank:Tank:1,tank:Other:1"),
            Err(RoleSpecError::DuplicateKey("tank".to_owned()))
        );
    }

    #[test]
    fn non_ecmascript_whitespace_is_preserved() {
        // NEL is Rust whitespace but not ECMAScript whitespace; neither are
        // Mongolian vowel separator and zero-width space.
        for ch in ['\u{0085}', '\u{180e}', '\u{200b}'] {
            let key = format!("{ch}tank{ch}");
            assert_eq!(
                parse_role_spec(&format!("{key}:Tank:2")),
                Err(RoleSpecError::BadKey(key)),
                "{ch:?}"
            );
            let label = format!("{ch}Tank{ch}");
            assert_eq!(
                parse_role_spec(&format!("tank:{label}:2")).expect("label preserved")[0].label,
                label,
                "{ch:?}"
            );
            assert_eq!(
                parse_role_spec(&format!("tank:Tank:{ch}2{ch}")),
                Err(RoleSpecError::BadSlots),
                "{ch:?}"
            );
            assert_eq!(validate_title(&ch.to_string()), Ok(ch.to_string()));
        }
    }

    #[test]
    fn ecmascript_trimming_preserves_interior_whitespace() {
        let roles = parse_role_spec("tank:Main\u{feff}Tank:2").expect("label preserved");
        assert_eq!(roles[0].label, "Main\u{feff}Tank");
        assert_eq!(
            validate_title("Friday\u{feff}raid"),
            Ok("Friday\u{feff}raid".to_owned())
        );
        assert!(parse_role_spec("ta\u{feff}nk:Tank:2").is_err());
        assert_eq!(
            parse_role_spec("tank:Tank:2\u{feff}0"),
            Err(RoleSpecError::BadSlots)
        );
    }

    #[test]
    fn role_spec_refusals_match_legacy() {
        // Not key:label:slots triplets.
        assert_eq!(parse_role_spec("tank:2"), Err(RoleSpecError::BadShape));
        assert_eq!(parse_role_spec("a:b:c:d"), Err(RoleSpecError::BadShape));
        assert_eq!(parse_role_spec(""), Err(RoleSpecError::BadShape));
        assert_eq!(
            parse_role_spec("tank:Tank:2,"),
            Err(RoleSpecError::BadShape)
        );
        // Bad keys (checked after trim+lowercase; message carries the raw field).
        assert_eq!(
            parse_role_spec("Tank!:Tank:2"),
            Err(RoleSpecError::BadKey("Tank!".to_owned()))
        );
        assert_eq!(
            parse_role_spec("UPPER:Tank:2").expect("upper is lowered")[0].key,
            "upper"
        );
        assert_eq!(
            parse_role_spec(":Tank:2"),
            Err(RoleSpecError::BadKey(String::new()))
        );
        assert_eq!(
            parse_role_spec(&format!("{}:Tank:2", "k".repeat(33))),
            Err(RoleSpecError::BadKey("k".repeat(33)))
        );
        // Bad labels.
        assert_eq!(parse_role_spec("tank::2"), Err(RoleSpecError::BadLabel));
        assert_eq!(
            parse_role_spec(&format!("tank:{}:2", "L".repeat(81))),
            Err(RoleSpecError::BadLabel)
        );
        // Bad slots: zero, over max, fractions, non-numeric, empty.
        for bad in [
            "tank:Tank:0",
            "tank:Tank:100",
            "tank:Tank:2.5",
            "tank:Tank:lots",
            "tank:Tank:",
        ] {
            assert_eq!(parse_role_spec(bad), Err(RoleSpecError::BadSlots), "{bad}");
        }
        // Count bounds.
        assert_eq!(
            parse_role_spec(&vec!["a:A:1"; 21].join(",")),
            Err(RoleSpecError::BadCount)
        );
        // Duplicates compare normalized keys.
        assert_eq!(
            parse_role_spec("tank:Tank:1,TANK:Other:1"),
            Err(RoleSpecError::DuplicateKey("tank".to_owned()))
        );
    }

    #[test]
    fn role_key_errors_echo_only_a_bounded_raw_prefix() {
        let raw_key = "k".repeat(MAX_ROLE_KEY_ERROR_CHARS + 64);
        let bounded_key = "k".repeat(MAX_ROLE_KEY_ERROR_CHARS);
        let error = parse_role_spec(&format!("{raw_key}:Tank:2")).unwrap_err();

        assert_eq!(error, RoleSpecError::BadKey(bounded_key.clone()));
        assert_eq!(
            error.to_string(),
            format!("Invalid LFG role key \"{bounded_key}\".")
        );
    }

    #[test]
    fn role_spec_rejects_input_over_the_published_utf16_bound() {
        assert_eq!(
            parse_role_spec(&"x".repeat(MAX_ROLE_SPEC_CHARS + 1)),
            Err(RoleSpecError::TooLong)
        );
        assert_eq!(
            parse_role_spec(&"😀".repeat(MAX_ROLE_SPEC_CHARS / 2 + 1)),
            Err(RoleSpecError::TooLong)
        );
    }

    #[test]
    fn role_spec_refuses_reserved_leave_key() {
        let reserved = RoleSpecError::ReservedKey(LFG_LEAVE_VALUE.to_owned());
        // Refused in every position.
        assert_eq!(parse_role_spec("__leave__:Leave:1"), Err(reserved.clone()));
        assert_eq!(
            parse_role_spec("tank:Tank:1,__leave__:Leave:1"),
            Err(reserved.clone())
        );
        assert_eq!(
            parse_role_spec("tank:Tank:1,__leave__:Leave:1,dps:DPS:1"),
            Err(reserved.clone())
        );
        assert_eq!(
            parse_role_spec("tank:Tank:1,dps:DPS:1,__leave__:Leave:1"),
            Err(reserved.clone())
        );
        // Case and surrounding-whitespace variants normalize to the sentinel.
        for raw in [
            "__LEAVE__",
            "__Leave__",
            "__lEaVe__",
            " __leave__ ",
            "\t__LEAVE__\n",
            "\u{feff}__leave__\u{feff}",
        ] {
            assert_eq!(
                parse_role_spec(&format!("{raw}:Leave:1")),
                Err(reserved.clone()),
                "{raw:?}"
            );
            assert_eq!(
                parse_role_spec(&format!("tank:Tank:1,{raw}:Leave:1")),
                Err(reserved.clone()),
                "{raw:?}"
            );
        }
        // Near-sentinel keys stay valid, keeping the leave action distinct.
        let roles = parse_role_spec("tank:Tank:1,leave:Leave:1,__leave___:Near:1").expect("parses");
        assert_eq!(
            roles
                .iter()
                .map(|role| role.key.as_str())
                .collect::<Vec<_>>(),
            ["tank", "leave", "__leave___"]
        );
        // Duplicate detection is unchanged: plain duplicates still report the
        // normalized key, and the reserved key refuses before dedup can fire.
        assert_eq!(
            parse_role_spec("tank:Tank:1,TANK:Other:1"),
            Err(RoleSpecError::DuplicateKey("tank".to_owned()))
        );
        assert_eq!(
            parse_role_spec("__leave__:Leave:1,__leave__:Leave:1"),
            Err(reserved.clone())
        );
    }

    #[test]
    fn role_spec_numbers_and_unicode_match_javascript() {
        for number in ["2", "2.0", "2e0", "+2", "0x2", "0X2", "0b10", "0o2"] {
            assert_eq!(
                parse_role_spec(&format!("tank:Tank:{number}")).expect("legacy number")[0].slots,
                2
            );
        }
        for number in ["NaN", "Infinity", "-Infinity", "-0x2", "0b2", "0o8"] {
            assert_eq!(
                parse_role_spec(&format!("tank:Tank:{number}")),
                Err(RoleSpecError::BadSlots)
            );
        }
        assert!(parse_role_spec(&format!("tank:{}:1", "😀".repeat(40))).is_ok());
        assert_eq!(
            parse_role_spec(&format!("tank:{}:1", "😀".repeat(41))),
            Err(RoleSpecError::BadLabel)
        );
        assert!(validate_title(&"😀".repeat(50)).is_ok());
        assert_eq!(validate_title(&"😀".repeat(51)), Err(TitleError::BadTitle));
    }

    #[test]
    fn role_spec_error_messages_match_legacy() {
        assert_eq!(
            RoleSpecError::BadShape.to_string(),
            "roles must be key:label:slots entries separated by commas."
        );
        assert_eq!(
            RoleSpecError::BadKey("Tank!".to_owned()).to_string(),
            "Invalid LFG role key \"Tank!\"."
        );
        assert_eq!(
            RoleSpecError::BadLabel.to_string(),
            "LFG role labels must be 1-80 characters."
        );
        assert_eq!(
            RoleSpecError::BadSlots.to_string(),
            "LFG role slots must be integers from 1-99."
        );
        assert_eq!(RoleSpecError::BadCount.to_string(), "LFG needs 1-20 roles.");
        assert_eq!(
            RoleSpecError::DuplicateKey("tank".to_owned()).to_string(),
            "Duplicate LFG role key \"tank\"."
        );
        assert_eq!(
            RoleSpecError::ReservedKey(LFG_LEAVE_VALUE.to_owned()).to_string(),
            "LFG role key \"__leave__\" is reserved for leaving the group."
        );
    }

    #[test]
    fn title_bounds_match_legacy() {
        assert_eq!(
            validate_title("  Friday raid  "),
            Ok("Friday raid".to_owned())
        );
        assert_eq!(validate_title("   "), Err(TitleError::BadTitle));
        assert_eq!(validate_title(""), Err(TitleError::BadTitle));
        assert_eq!(validate_title(&"x".repeat(101)), Err(TitleError::BadTitle));
        assert!(validate_title(&"x".repeat(100)).is_ok());
        assert_eq!(
            TitleError::BadTitle.to_string(),
            "LFG title must be 1-100 characters."
        );
    }

    #[test]
    fn starts_at_requires_future_iso8601() {
        let now_ms = 1_789_034_400_000; // 2026-09-10T10:00:00Z
                                        // Normalized to UTC millis form.
        assert_eq!(
            normalize_starts_at("2026-09-11T20:00:00Z", now_ms),
            Ok("2026-09-11T20:00:00.000Z".to_owned())
        );
        // Offsets shift to UTC.
        assert_eq!(
            normalize_starts_at("2026-09-11T22:00:00+02:00", now_ms),
            Ok("2026-09-11T20:00:00.000Z".to_owned())
        );
        // Past and present are refused.
        assert_eq!(
            normalize_starts_at("2026-09-10T10:00:00Z", now_ms),
            Err(StartsAtError::NotFuture)
        );
        assert_eq!(
            normalize_starts_at("2020-01-01T00:00:00Z", now_ms),
            Err(StartsAtError::NotFuture)
        );
        // Non-ISO input is refused (legacy Date.parse would accept some of
        // these; the port only honors the documented ISO-8601 contract).
        for bad in ["tomorrow", "2026-09-11 20:00:00", "not-a-date", ""] {
            assert_eq!(
                normalize_starts_at(bad, now_ms),
                Err(StartsAtError::NotIso8601),
                "{bad:?}"
            );
        }
        assert_eq!(
            StartsAtError::NotIso8601.to_string(),
            "starts-at must be an ISO-8601 timestamp."
        );
        assert_eq!(
            StartsAtError::NotFuture.to_string(),
            "starts-at must be in the future."
        );
    }

    #[test]
    fn adjudication_order_matches_legacy_transaction() {
        // Missing post.
        assert_eq!(
            adjudicate_signup(false, LfgStatus::Open, Some(1), None, "tank", 0),
            SignupOutcome::Missing
        );
        // Closed post locks signups.
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Closed, Some(1), None, "tank", 0),
            SignupOutcome::Closed
        );
        // Unknown role.
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, None, None, "nope", 0),
            SignupOutcome::Missing
        );
        // Same-role re-signup reports joined without consulting capacity.
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, Some(1), Some("tank"), "tank", 1),
            SignupOutcome::Joined
        );
        // Full role (used excludes the requester, per the store count).
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, Some(1), None, "tank", 1),
            SignupOutcome::Full
        );
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, Some(1), Some("dps"), "tank", 1),
            SignupOutcome::Full
        );
        // Switch vs first join.
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, Some(2), Some("dps"), "tank", 0),
            SignupOutcome::Moved
        );
        assert_eq!(
            adjudicate_signup(true, LfgStatus::Open, Some(2), None, "tank", 0),
            SignupOutcome::Joined
        );
    }

    fn sample_post(status: LfgStatus) -> LfgPost {
        LfgPost {
            id: "lfg-proof".to_owned(),
            guild_id: "111111111111111111".to_owned(),
            channel_id: "222222222222222222".to_owned(),
            message_id: Some("333333333333333333".to_owned()),
            title: "Friday raid".to_owned(),
            starts_at: "2026-09-11T20:00:00.000Z".to_owned(),
            status,
            created_by: "444444444444444444".to_owned(),
            created_at: "2026-09-10T10:00:00.000Z".to_owned(),
            closed_at: None,
        }
    }

    fn sample_roles() -> Vec<LfgRole> {
        spec_roles(
            "lfg-proof",
            &parse_role_spec("tank:Tank:1,dps:DPS:1").expect("parses"),
        )
    }

    #[test]
    fn rendered_unicode_stays_within_legacy_limits() {
        assert_eq!(truncate_utf16("x😀y", 2), "x");
        assert_eq!(truncate_utf16("x😀y", 3), "x😀");
        let mut post = sample_post(LfgStatus::Open);
        post.title = "😀".repeat(50);
        let spec = (0..20)
            .map(|i| format!("r{i}:{}:2", "😀".repeat(40)))
            .collect::<Vec<_>>()
            .join(",");
        let roles = spec_roles(&post.id, &parse_role_spec(&spec).expect("valid roles"));
        let signups = roles
            .iter()
            .map(|role| LfgSignup {
                lfg_id: post.id.clone(),
                user_id: "555555555555555555".to_owned(),
                role_key: role.role_key.clone(),
                joined_at: post.created_at.clone(),
            })
            .collect::<Vec<_>>();
        let content = lfg_content(&post, &roles, &signups);
        assert!((1999..=2000).contains(&content.encode_utf16().count()));
        assert!(lfg_select_options(LfgStatus::Open, &roles, &signups)
            .iter()
            .all(|option| option.label.encode_utf16().count() <= MAX_OPTION_LABEL_CHARS));
    }

    #[test]
    fn content_matches_legacy_shape() {
        let signups = vec![LfgSignup {
            lfg_id: "lfg-proof".to_owned(),
            user_id: "555555555555555555".to_owned(),
            role_key: "dps".to_owned(),
            joined_at: "2026-09-10T11:00:00.000Z".to_owned(),
        }];
        let content = lfg_content(&sample_post(LfgStatus::Open), &sample_roles(), &signups);
        // 2026-09-11T20:00:00Z = 1789156800.
        assert!(content.starts_with("**Friday raid** — Open\nStarts <t:1789156800:F>\n"));
        assert!(content.contains("**Tank** 0/1\n"));
        assert!(content.contains("**DPS** 1/1 — <@555555555555555555>"));
        let closed = lfg_content(&sample_post(LfgStatus::Closed), &sample_roles(), &signups);
        assert!(closed.contains("— Closed\n"));
    }

    #[test]
    fn select_options_carry_fill_and_leave() {
        let options = lfg_select_options(LfgStatus::Open, &sample_roles(), &[]);
        assert_eq!(
            options,
            vec![
                LfgSelectOption {
                    label: "Tank (0/1)".to_owned(),
                    value: "tank".to_owned()
                },
                LfgSelectOption {
                    label: "DPS (0/1)".to_owned(),
                    value: "dps".to_owned()
                },
                LfgSelectOption {
                    label: "Leave this group".to_owned(),
                    value: "__leave__".to_owned()
                },
            ]
        );
        // Closed posts render no components.
        assert!(lfg_select_options(LfgStatus::Closed, &sample_roles(), &[]).is_empty());
    }

    #[test]
    fn select_parsing_routes_signup_and_leave() {
        assert_eq!(
            parse_lfg_select("two:lfg:lfg-proof", "tank"),
            Some(LfgSelectAction::Signup {
                post_id: "lfg-proof".to_owned(),
                role_key: "tank".to_owned(),
            })
        );
        assert_eq!(
            parse_lfg_select("two:lfg:lfg-proof", "__leave__"),
            Some(LfgSelectAction::Leave {
                post_id: "lfg-proof".to_owned()
            })
        );
        // Unknown values still route to signup (the store answers `missing`).
        assert_eq!(
            parse_lfg_select("two:lfg:lfg-proof", ""),
            Some(LfgSelectAction::Signup {
                post_id: "lfg-proof".to_owned(),
                role_key: String::new(),
            })
        );
        assert_eq!(parse_lfg_select("two:other:x", "tank"), None);
        assert_eq!(lfg_custom_id("lfg-proof"), "two:lfg:lfg-proof");
    }

    #[test]
    fn permission_gate_matches_legacy_runtime_check() {
        assert!(require_manage_events(PERM_MANAGE_EVENTS).is_ok());
        // Extra permission bits do not matter (ManageEvents is bit 33).
        assert!(require_manage_events(PERM_MANAGE_EVENTS | 8).is_ok());
        assert!(require_manage_events(8).is_err());
        assert_eq!(
            require_manage_events(0),
            Err(LfgPermissionError::ManageEventsRequired)
        );
        assert_eq!(
            LfgPermissionError::ManageEventsRequired.to_string(),
            "Manage Events permission is required."
        );
    }

    #[test]
    fn reply_texts_match_legacy() {
        assert_eq!(created_reply("lfg-proof"), "LFG posted: `lfg-proof`.");
        assert_eq!(signup_reply(SignupOutcome::Joined), "LFG joined.");
        assert_eq!(signup_reply(SignupOutcome::Moved), "LFG moved.");
        assert_eq!(signup_reply(SignupOutcome::Full), "LFG full.");
        assert_eq!(signup_reply(SignupOutcome::Closed), "LFG closed.");
        assert_eq!(signup_reply(SignupOutcome::Missing), "LFG missing.");
        assert_eq!(leave_reply(true), "LFG left.");
        assert_eq!(leave_reply(false), "LFG were not signed up.");
        assert_eq!(close_reply(true), "LFG closed.");
        assert_eq!(close_reply(false), "LFG was already closed or missing.");
    }

    #[test]
    fn nonce_matches_legacy_sha256_prefix() {
        // sha256("lfg\0lfg-proof") hex, first 24 chars — byte-exact with
        // legacy createHash('sha256').update(`lfg\0${id}`).
        assert_eq!(lfg_nonce("lfg-proof"), "3d8770e5c6149f62317074b7");
        assert_eq!(lfg_nonce("test-post"), "f06b900bb1593e4c41249874");
        for id in ["lfg-proof", "x", &"y".repeat(64)] {
            let nonce = lfg_nonce(id);
            assert_eq!(nonce.len(), 24);
            assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn status_round_trips() {
        assert_eq!(LfgStatus::from_stored("open"), Some(LfgStatus::Open));
        assert_eq!(LfgStatus::from_stored("closed"), Some(LfgStatus::Closed));
        assert_eq!(LfgStatus::from_stored("archived"), None);
        assert_eq!(LfgStatus::Open.as_str(), "open");
    }
}
