//! Pure lockdown-preview planner: snapshot validation, the single-role
//! projection and the stale-check digest for `/lockdown-preview`
//! (`docs/lockdown-preview-spec.md` sections 4-5, option B).
//!
//! This module is framework-free and performs no I/O. The Discord reads that
//! build a [`PreviewSnapshot`], the ephemeral render, and the confirm path
//! that re-plans and compares digests live in later slices; nothing here is
//! wired to a command yet. Callers must pass a reason already trimmed and
//! validated by `require_channel_reason`: the digest covers it exactly as
//! given, with no Unicode normalization.

use sha2::{Digest, Sha256};

use super::channel_moderation::{
    plan_lockdown, EveryoneOverwrite, LockdownPlan, MaskError, ADD_REACTIONS_BIT,
    CREATE_PRIVATE_THREADS_BIT, CREATE_PUBLIC_THREADS_BIT, LOCKDOWN_BITS, SEND_MESSAGES_BIT,
    SEND_MESSAGES_IN_THREADS_BIT,
};

pub const VIEW_CHANNEL_BIT: u64 = 1 << 10;
pub const ADMINISTRATOR_BIT: u64 = 1 << 3;
/// Bits the preview renders claims about, and the only role/overwrite bits
/// the digest covers unmasked (spec section 5).
pub const INTEREST_BITS: u64 = LOCKDOWN_BITS | VIEW_CHANNEL_BIT | ADMINISTRATOR_BIT;
pub const MAX_ROLES: usize = 250;
pub const MAX_OVERWRITES: usize = 100;
pub const DIGEST_VERSION: &str = "lockdown-preview.v1";
/// Lockdown bits in the fixed render order, with their Discord names.
pub const LOCKDOWN_BIT_NAMES: [(&str, u64); 5] = [
    ("SendMessages", SEND_MESSAGES_BIT),
    ("AddReactions", ADD_REACTIONS_BIT),
    ("CreatePublicThreads", CREATE_PUBLIC_THREADS_BIT),
    ("CreatePrivateThreads", CREATE_PRIVATE_THREADS_BIT),
    ("SendMessagesInThreads", SEND_MESSAGES_IN_THREADS_BIT),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSnapshot {
    pub id: u64,
    pub position: i64,
    pub permissions: u64,
}

/// One channel permission overwrite. `kind` is the Discord wire value:
/// `0` for a role overwrite, `1` for a member overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverwriteSnapshot {
    pub kind: u8,
    pub id: u64,
    pub allow: u64,
    pub deny: u64,
}

/// Everything one preview (or one confirm stale-check) read, exactly once.
/// The `@everyone` overwrite is the `kind == 0, id == guild_id` row of
/// `overwrites`, if present. `saved_generation` is `""` when no saved
/// lockdown record exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSnapshot {
    pub guild_id: u64,
    pub channel_id: u64,
    pub channel_type: u64,
    pub parent_id: Option<u64>,
    pub actor_id: u64,
    pub reason: String,
    pub saved_present: bool,
    pub saved_generation: String,
    pub roles: Vec<RoleSnapshot>,
    pub overwrites: Vec<OverwriteSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PreviewError {
    #[error("lockdown preview supports text and announcement channels only")]
    UnsupportedChannel,
    #[error("channel permission layout is too complex to preview safely")]
    TooComplex,
    #[error("role list does not contain the @everyone role")]
    MissingEveryoneRole,
    #[error("role list contains a duplicate role id")]
    DuplicateRole,
    #[error("channel contains a duplicate permission overwrite")]
    DuplicateOverwrite,
    #[error("permission overwrite has an unknown type")]
    InvalidOverwriteKind,
    #[error(transparent)]
    InvalidMask(#[from] MaskError),
}

/// How one bit sits in a pair of allow/deny masks. A bit set in both masks
/// is reported, never silently normalized (spec section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitState {
    Allow,
    Deny,
    Neutral,
    AllowDeny,
}

impl BitState {
    #[must_use]
    pub fn of(allow: u64, deny: u64, bit: u64) -> Self {
        match (allow & bit != 0, deny & bit != 0) {
            (true, true) => Self::AllowDeny,
            (true, false) => Self::Allow,
            (false, true) => Self::Deny,
            (false, false) => Self::Neutral,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Neutral => "neutral",
            Self::AllowDeny => "allow+deny",
        }
    }
}

/// Per-bit projection: the bit's state on the current `@everyone`
/// overwrite, and how many single-role projections lose/keep usable access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitProjection {
    pub name: &'static str,
    pub bit: u64,
    pub state: BitState,
    pub losing: usize,
    pub keeping: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPlan {
    pub lockdown: LockdownPlan,
    /// False exactly when the planned write equals the current masks (an
    /// absent overwrite counts as `(0, 0)`): nothing would change, so the
    /// preview must render without a Confirm button (spec section 3 step 6).
    pub armed: bool,
    pub prior_exists: bool,
    pub prior_allow: u64,
    pub prior_deny: u64,
    pub planned_allow: u64,
    pub planned_deny: u64,
    pub bits: [BitProjection; 5],
    pub everyone_send_state: BitState,
    pub view_state: BitState,
    /// Set bits outside `LOCKDOWN_BITS` on the current `@everyone` masks.
    pub preserved_allow_bits: u32,
    pub preserved_deny_bits: u32,
    pub roles_evaluated: usize,
    pub would_lose_send: usize,
    pub would_keep_send: usize,
    pub cannot_send_already: usize,
    /// Roles whose projection keeps usable SendMessages, position
    /// descending then id ascending (numeric). Complete, not truncated.
    pub keeper_role_ids: Vec<u64>,
    /// Member overwrites allowing at least one lockdown bit. Members are
    /// counted, never identified.
    pub member_overwrite_allows: usize,
    /// Roles whose projected base (`@everyone` | role) holds Administrator.
    pub administrator_roles: usize,
    /// Full SHA-256 hex digest of the canonical snapshot. Buttons carry
    /// only its first 32 hex characters; the full value stays in memory.
    pub digest: String,
}

impl PreviewPlan {
    #[must_use]
    pub fn digest32(&self) -> &str {
        &self.digest[..32]
    }
}

/// A bit is usable only with view. Mirrors the `channel_access.rs`
/// ordering for a synthetic member holding only one role: base, then the
/// `@everyone` overwrite, then that role's own overwrite.
fn usable(
    base: u64,
    everyone: Option<(u64, u64)>,
    role: Option<&OverwriteSnapshot>,
    bit: u64,
) -> bool {
    if base & ADMINISTRATOR_BIT != 0 {
        return true;
    }
    let mut permissions = base;
    if let Some((allow, deny)) = everyone {
        permissions = (permissions & !deny) | allow;
    }
    if let Some(overwrite) = role {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }
    permissions & VIEW_CHANNEL_BIT != 0 && permissions & bit != 0
}

/// Validate one snapshot and plan the preview. Any completeness failure
/// refuses the whole preview; no partial plan is ever returned.
pub fn plan_preview(snapshot: &PreviewSnapshot) -> Result<PreviewPlan, PreviewError> {
    if !matches!(snapshot.channel_type, 0 | 5) {
        return Err(PreviewError::UnsupportedChannel);
    }
    if snapshot.roles.len() > MAX_ROLES || snapshot.overwrites.len() > MAX_OVERWRITES {
        return Err(PreviewError::TooComplex);
    }
    let mut role_ids: Vec<u64> = snapshot.roles.iter().map(|role| role.id).collect();
    role_ids.sort_unstable();
    if role_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PreviewError::DuplicateRole);
    }
    let everyone_role = snapshot
        .roles
        .iter()
        .find(|role| role.id == snapshot.guild_id)
        .ok_or(PreviewError::MissingEveryoneRole)?;
    let mut overwrite_keys: Vec<(u8, u64)> = Vec::with_capacity(snapshot.overwrites.len());
    for overwrite in &snapshot.overwrites {
        if overwrite.kind > 1 {
            return Err(PreviewError::InvalidOverwriteKind);
        }
        overwrite_keys.push((overwrite.kind, overwrite.id));
    }
    overwrite_keys.sort_unstable();
    if overwrite_keys.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PreviewError::DuplicateOverwrite);
    }

    let everyone_overwrite = snapshot
        .overwrites
        .iter()
        .find(|overwrite| overwrite.kind == 0 && overwrite.id == snapshot.guild_id);
    let (prior_allow, prior_deny, prior_exists) = match everyone_overwrite {
        Some(overwrite) => (overwrite.allow, overwrite.deny, true),
        None => (0, 0, false),
    };
    let current = prior_exists.then(|| EveryoneOverwrite {
        allow: prior_allow.to_string(),
        deny: prior_deny.to_string(),
    });
    let lockdown = plan_lockdown(current.as_ref())?;
    // plan_lockdown only emits masks it formatted from parsed u64s.
    let planned_allow = lockdown.write.allow.parse::<u64>().map_err(|_| MaskError)?;
    let planned_deny = lockdown.write.deny.parse::<u64>().map_err(|_| MaskError)?;
    let armed = (planned_allow, planned_deny) != (prior_allow, prior_deny);

    let before = prior_exists.then_some((prior_allow, prior_deny));
    let after = Some((planned_allow, planned_deny));
    let mut bits = LOCKDOWN_BIT_NAMES.map(|(name, bit)| BitProjection {
        name,
        bit,
        state: BitState::of(prior_allow, prior_deny, bit),
        losing: 0,
        keeping: 0,
    });
    let mut cannot_send_already = 0usize;
    let mut keepers: Vec<&RoleSnapshot> = Vec::new();
    let mut administrator_roles = 0usize;
    for role in &snapshot.roles {
        let base = everyone_role.permissions | role.permissions;
        if base & ADMINISTRATOR_BIT != 0 {
            administrator_roles += 1;
        }
        let own = (role.id != snapshot.guild_id)
            .then(|| {
                snapshot
                    .overwrites
                    .iter()
                    .find(|overwrite| overwrite.kind == 0 && overwrite.id == role.id)
            })
            .flatten();
        for projection in &mut bits {
            let was = usable(base, before, own, projection.bit);
            let will = usable(base, after, own, projection.bit);
            if was && will {
                projection.keeping += 1;
            } else if was {
                projection.losing += 1;
            }
            if projection.bit == SEND_MESSAGES_BIT {
                if !was {
                    cannot_send_already += 1;
                }
                if will {
                    keepers.push(role);
                }
            }
        }
    }
    keepers.sort_by(|a, b| b.position.cmp(&a.position).then(a.id.cmp(&b.id)));

    let canonical = canonical_bytes(
        snapshot,
        prior_exists,
        prior_allow,
        prior_deny,
        planned_allow,
        planned_deny,
    );
    Ok(PreviewPlan {
        lockdown,
        armed,
        prior_exists,
        prior_allow,
        prior_deny,
        planned_allow,
        planned_deny,
        everyone_send_state: BitState::of(prior_allow, prior_deny, SEND_MESSAGES_BIT),
        view_state: BitState::of(prior_allow, prior_deny, VIEW_CHANNEL_BIT),
        preserved_allow_bits: (prior_allow & !LOCKDOWN_BITS).count_ones(),
        preserved_deny_bits: (prior_deny & !LOCKDOWN_BITS).count_ones(),
        roles_evaluated: snapshot.roles.len(),
        would_lose_send: bits[0].losing,
        would_keep_send: bits[0].keeping,
        cannot_send_already,
        keeper_role_ids: keepers.iter().map(|role| role.id).collect(),
        member_overwrite_allows: snapshot
            .overwrites
            .iter()
            .filter(|overwrite| overwrite.kind == 1 && overwrite.allow & LOCKDOWN_BITS != 0)
            .count(),
        administrator_roles,
        digest: hex::encode(Sha256::digest(canonical)),
        bits,
    })
}

/// Append `value` as a JSON string escaped exactly like Python
/// `json.dumps(..., ensure_ascii=True)`: printable ASCII `0x20..=0x7E`
/// passes through, everything else becomes `\uXXXX` per UTF-16 code unit
/// (astral characters as surrogate pairs). No Unicode normalization.
fn escape_json_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\u{20}'..='\u{7E}' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn push_string_field(out: &mut String, value: &str) {
    if !out.ends_with('[') {
        out.push(',');
    }
    escape_json_string(value, out);
}

/// Canonical snapshot bytes: the section 5 JSON array with decimal-string
/// integers, compact separators, roles sorted by numeric id, overwrites by
/// numeric `(type, id)`, role/overwrite masks reduced to `INTEREST_BITS`.
fn canonical_bytes(
    snapshot: &PreviewSnapshot,
    prior_exists: bool,
    prior_allow: u64,
    prior_deny: u64,
    planned_allow: u64,
    planned_deny: u64,
) -> Vec<u8> {
    let mut out = String::from("[");
    for field in [
        DIGEST_VERSION.to_owned(),
        snapshot.guild_id.to_string(),
        snapshot.channel_id.to_string(),
        snapshot.channel_type.to_string(),
        snapshot.parent_id.unwrap_or(0).to_string(),
        snapshot.actor_id.to_string(),
        snapshot.reason.clone(),
        u8::from(prior_exists).to_string(),
        prior_allow.to_string(),
        prior_deny.to_string(),
        planned_allow.to_string(),
        planned_deny.to_string(),
        u8::from(snapshot.saved_present).to_string(),
        snapshot.saved_generation.clone(),
    ] {
        push_string_field(&mut out, &field);
    }
    let mut roles: Vec<&RoleSnapshot> = snapshot.roles.iter().collect();
    roles.sort_by_key(|role| role.id);
    out.push_str(",[");
    for (index, role) in roles.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        push_string_field(&mut out, &role.id.to_string());
        push_string_field(&mut out, &role.position.to_string());
        push_string_field(&mut out, &(role.permissions & INTEREST_BITS).to_string());
        out.push(']');
    }
    let mut overwrites: Vec<&OverwriteSnapshot> = snapshot.overwrites.iter().collect();
    overwrites.sort_by_key(|overwrite| (overwrite.kind, overwrite.id));
    out.push_str("],[");
    for (index, overwrite) in overwrites.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        push_string_field(&mut out, &overwrite.kind.to_string());
        push_string_field(&mut out, &overwrite.id.to_string());
        push_string_field(&mut out, &(overwrite.allow & INTEREST_BITS).to_string());
        push_string_field(&mut out, &(overwrite.deny & INTEREST_BITS).to_string());
        out.push(']');
    }
    out.push_str("]]");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: u64 = VIEW_CHANNEL_BIT;
    const SEND: u64 = SEND_MESSAGES_BIT;
    const EMBED: u64 = 1 << 14;

    fn role(id: u64, position: i64, permissions: u64) -> RoleSnapshot {
        RoleSnapshot {
            id,
            position,
            permissions,
        }
    }

    fn ow(kind: u8, id: u64, allow: u64, deny: u64) -> OverwriteSnapshot {
        OverwriteSnapshot {
            kind,
            id,
            allow,
            deny,
        }
    }

    fn snapshot(roles: Vec<RoleSnapshot>, overwrites: Vec<OverwriteSnapshot>) -> PreviewSnapshot {
        PreviewSnapshot {
            guild_id: 10,
            channel_id: 20,
            channel_type: 0,
            parent_id: None,
            actor_id: 30,
            reason: "raid".to_owned(),
            saved_present: false,
            saved_generation: String::new(),
            roles,
            overwrites,
        }
    }

    fn digest_of(snapshot: &PreviewSnapshot, prior_exists: bool) -> String {
        hex::encode(Sha256::digest(canonical_bytes(
            snapshot,
            prior_exists,
            0,
            0,
            0,
            LOCKDOWN_BITS,
        )))
    }

    #[test]
    fn canonical_bytes_and_digest_reproduce_the_spec_oracle_vectors() {
        // The doctest in docs/lockdown-preview-spec.md section 8.
        let snap = snapshot(vec![role(10, 0, VIEW | SEND)], vec![ow(0, 40, SEND, 0)]);
        assert_eq!(
            canonical_bytes(&snap, true, 0, 0, 0, LOCKDOWN_BITS),
            br#"["lockdown-preview.v1","10","20","0","0","30","raid","1","0","0","0","377957124160","0","",[["10","0","3072"]],[["0","40","2048","0"]]]"#
        );
        assert_eq!(
            digest_of(&snap, true),
            "fe0db22860adfe223f7f5b5f38357188b74bb422ae49628c6a01bc07ab8ace05"
        );
        let mut uni = snap.clone();
        uni.reason = "raid 🛡️".to_owned();
        assert!(
            String::from_utf8(canonical_bytes(&uni, true, 0, 0, 0, LOCKDOWN_BITS))
                .expect("ascii bytes")
                .contains(r#""raid \ud83d\udee1\ufe0f""#)
        );
        assert_eq!(
            digest_of(&uni, true),
            "81a08dd5255d466c7a41230d3b15cdf46b73d978a8953ff91ee852ded68cd8f8"
        );
        // No Unicode normalization: composed and decomposed differ.
        let mut nfd = snap.clone();
        nfd.reason = "cafe\u{301}".to_owned();
        let mut nfc = snap;
        nfc.reason = "caf\u{e9}".to_owned();
        assert_ne!(digest_of(&nfc, true), digest_of(&nfd, true));
    }

    #[test]
    fn digest_sorts_numerically_and_ignores_non_interest_bits() {
        let base = snapshot(
            vec![role(10, 0, VIEW | SEND), role(9, 1, 0)],
            vec![ow(1, 9, 0, 0), ow(0, 10, 0, 0), ow(0, 9, 0, 0)],
        );
        let mut shuffled = base.clone();
        shuffled.roles.reverse();
        shuffled.overwrites.reverse();
        let mut embed_only = base.clone();
        embed_only.roles[0].permissions |= EMBED;
        embed_only.overwrites[0].allow |= EMBED;
        let plan = plan_preview(&base).expect("plans");
        assert_eq!(plan.digest, plan_preview(&shuffled).expect("plans").digest);
        assert_eq!(
            plan.digest,
            plan_preview(&embed_only).expect("plans").digest
        );
        // An INTEREST change, a position change, or an @everyone VIEW flip is stale.
        let edits: [fn(&mut PreviewSnapshot); 3] = [
            |snap| snap.roles[0].permissions = VIEW,
            |snap| snap.roles[1].position = 7,
            |snap| snap.overwrites[1].allow = VIEW,
        ];
        for edit in edits {
            let mut changed = base.clone();
            edit(&mut changed);
            assert_ne!(plan.digest, plan_preview(&changed).expect("plans").digest);
        }
        assert_eq!(plan.digest32().len(), 32);
    }

    #[test]
    fn absent_overwrite_plans_a_created_deny_and_arms() {
        let snap = snapshot(vec![role(10, 0, VIEW | SEND)], vec![]);
        let plan = plan_preview(&snap).expect("plans");
        assert!(!plan.prior_exists && plan.armed);
        assert_eq!(
            (plan.planned_allow, plan.planned_deny),
            (0, 377_957_124_160)
        );
        assert_eq!(plan.lockdown.write.deny, LOCKDOWN_BITS.to_string());
        assert_eq!((plan.would_lose_send, plan.would_keep_send), (1, 0));
    }

    #[test]
    fn unrelated_everyone_bits_are_preserved_and_counted() {
        let snap = snapshot(vec![role(10, 0, 0)], vec![ow(0, 10, VIEW | EMBED, 1 << 13)]);
        let plan = plan_preview(&snap).expect("plans");
        assert_eq!(plan.planned_allow, VIEW | EMBED);
        assert_eq!(plan.planned_deny, (1 << 13) | LOCKDOWN_BITS);
        assert_eq!(
            (plan.preserved_allow_bits, plan.preserved_deny_bits),
            (2, 1)
        );
        assert_eq!(plan.view_state, BitState::Allow);
    }

    #[test]
    fn allow_plus_deny_is_reported_and_counts_as_usable_before() {
        let snap = snapshot(
            vec![role(10, 0, VIEW)],
            vec![ow(0, 10, SEND, LOCKDOWN_BITS)],
        );
        let plan = plan_preview(&snap).expect("plans");
        assert!(plan.armed);
        assert_eq!(plan.everyone_send_state, BitState::AllowDeny);
        assert_eq!(plan.everyone_send_state.name(), "allow+deny");
        assert_eq!((plan.would_lose_send, plan.would_keep_send), (1, 0));
    }

    #[test]
    fn fully_locked_is_unarmed_and_a_partial_lock_is_armed() {
        for (allow, deny, armed) in [
            (0, LOCKDOWN_BITS, false),
            (0, SEND, true),
            (SEND, LOCKDOWN_BITS, true),
        ] {
            let snap = snapshot(vec![role(10, 0, 0)], vec![ow(0, 10, allow, deny)]);
            assert_eq!(plan_preview(&snap).expect("plans").armed, armed);
        }
    }

    #[test]
    fn role_overwrite_keeps_send_member_overwrite_is_only_counted() {
        let snap = snapshot(
            vec![role(10, 0, VIEW | SEND), role(40, 5, 0), role(50, 9, 0)],
            vec![
                ow(0, 40, SEND, 0),
                ow(0, 50, SEND, 0),
                ow(1, 99, SEND_MESSAGES_IN_THREADS_BIT, 0),
                ow(1, 98, EMBED, 0),
            ],
        );
        let plan = plan_preview(&snap).expect("plans");
        // Position descending, then id ascending.
        assert_eq!(plan.keeper_role_ids, [50, 40]);
        assert_eq!(plan.would_keep_send, 2);
        assert_eq!(plan.member_overwrite_allows, 1);
        assert_eq!(plan.bits[0].name, "SendMessages");
    }

    #[test]
    fn administrator_bypasses_and_missing_view_gates_send() {
        let snap = snapshot(
            vec![
                role(10, 0, SEND),
                role(40, 1, ADMINISTRATOR_BIT),
                role(50, 2, 0),
            ],
            vec![ow(0, 10, 0, VIEW)],
        );
        let plan = plan_preview(&snap).expect("plans");
        assert_eq!(plan.administrator_roles, 1);
        assert_eq!(plan.keeper_role_ids, [40]);
        assert_eq!(plan.cannot_send_already, 2);
    }

    #[test]
    fn completeness_gates_refuse_without_a_partial_plan() {
        let cases: Vec<(PreviewSnapshot, PreviewError)> = vec![
            (
                snapshot(vec![role(11, 0, 0)], vec![]),
                PreviewError::MissingEveryoneRole,
            ),
            (
                snapshot(vec![role(10, 0, 0), role(10, 1, 0)], vec![]),
                PreviewError::DuplicateRole,
            ),
            (
                snapshot(vec![role(10, 0, 0)], vec![ow(0, 40, 0, 0), ow(0, 40, 1, 0)]),
                PreviewError::DuplicateOverwrite,
            ),
            (
                snapshot(vec![role(10, 0, 0)], vec![ow(2, 40, 0, 0)]),
                PreviewError::InvalidOverwriteKind,
            ),
            (
                snapshot((10..=260).map(|id| role(id, 0, 0)).collect(), vec![]),
                PreviewError::TooComplex,
            ),
            (
                snapshot(
                    vec![role(10, 0, 0)],
                    (0..101).map(|id| ow(1, 1000 + id, 0, 0)).collect(),
                ),
                PreviewError::TooComplex,
            ),
        ];
        for (snap, error) in cases {
            assert_eq!(plan_preview(&snap), Err(error));
        }
        let mut voice = snapshot(vec![role(10, 0, 0)], vec![]);
        voice.channel_type = 2;
        assert_eq!(plan_preview(&voice), Err(PreviewError::UnsupportedChannel));
        voice.channel_type = 5;
        assert!(plan_preview(&voice).is_ok());
    }
}
