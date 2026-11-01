//! `/lockdown-preview` slash state: bounded pending store, rate limiter and
//! ephemeral render (`docs/lockdown-preview-spec.md` sections 3-6, option B,
//! build slice 4).
//!
//! The preview slash (wired in `channel_moderation.rs`) renders the exact
//! `@everyone` diff and arms one Confirm button. Confirm/cancel component
//! presses arrive in a later slice; this module owns everything the slash
//! needs: the in-memory pending map, the in-memory rate limiter, the fixed
//! render, the button ids and the numbers-only audit metadata. Nothing here
//! is published: the command definition, help and docs land in the
//! publication slice. The pure planner (`two_bot_core::lockdown_preview`)
//! owns the snapshot gates and the digest.
//!
//! All bounds are proposed constants from spec section 5, and all clocks are
//! caller-supplied epoch seconds so tests pin the TTL and window edges
//! without sleeping.

use std::collections::{HashMap, VecDeque};

use two_bot_core::lockdown_preview::PreviewPlan;
use two_bot_core::message_safety::neutralize_mentions;

/// Seconds a rendered armed preview stays consumable (spec section 5).
pub const PREVIEW_TTL_SECS: i64 = 300;
/// Process-wide cap on live armed previews; a new key at cap refuses with
/// `PREVIEW_BUSY` instead of evicting someone else's preview.
pub const MAX_PENDING: usize = 64;
/// Sliding-window budget per previewing actor.
pub const MAX_PREVIEWS_PER_ACTOR: usize = 10;
/// Sliding-window budget per guild.
pub const MAX_PREVIEWS_PER_GUILD: usize = 30;
/// Rate-window length in seconds (spec section 5: 10 minutes).
pub const RATE_WINDOW_SECS: i64 = 600;
/// Repository message-length convention: the render must fit under the
/// 2,000-unit Discord limit with margin (spec section 4).
pub const MAX_RENDER_UTF16: usize = 1900;
/// Keeper role tokens named before the count-only fallback (spec section 4).
pub const MAX_KEEPER_NAMES: usize = 10;

/// Outcome token for an armed-preview confirm/cancel press against a missing
/// or consumed entry. The confirm slice owns the collapsed reply; the slash
/// only needs the token for button construction tests.
pub const PREVIEW_EXPIRED: &str = "PREVIEW_EXPIRED";

/// One outstanding armed preview per `(guild, channel, actor)`. The full
/// digest stays in memory; buttons carry only its first 32 hex characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    pub digest: String,
    pub reason: String,
    pub preview_request_id: String,
    pub expires_at: i64,
}

type ActorKey = (u64, u64, u64);

/// Bounded in-memory pending map with TTL, consume-once take and atomic
/// same-key replacement (spec section 5: a replacement succeeds even at cap
/// and the superseded digest is never consumable afterwards).
#[derive(Debug, Default)]
pub struct PendingStore {
    entries: HashMap<ActorKey, PendingEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingFull;

impl PendingStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn sweep(&mut self, now: i64) {
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    /// Store `entry` for `key`, superseding that actor's previous digest for
    /// the channel. Expired entries are swept first and the superseded entry
    /// is removed before the cap is evaluated, so a same-key replacement
    /// never counts as a 65th entry. A new key at cap refuses with
    /// [`PendingFull`] and leaves every live entry intact.
    pub fn insert(
        &mut self,
        key: ActorKey,
        entry: PendingEntry,
        now: i64,
    ) -> Result<(), PendingFull> {
        self.sweep(now);
        self.entries.remove(&key);
        if self.entries.len() >= MAX_PENDING {
            return Err(PendingFull);
        }
        self.entries.insert(key, entry);
        Ok(())
    }

    /// Consume the entry for `key` whose digest starts with `digest32`.
    /// The button carries exactly 32 hex characters; anything shorter,
    /// longer or non-hex refuses without consuming, so a malformed press
    /// collapses to `PREVIEW_EXPIRED` instead of eating a live entry. A
    /// foreign digest, a double press or an expired entry also yields `None`.
    pub fn take(&mut self, key: ActorKey, digest32: &str, now: i64) -> Option<PendingEntry> {
        self.sweep(now);
        let well_formed =
            digest32.len() == 32 && digest32.bytes().all(|byte| byte.is_ascii_hexdigit());
        let matches = well_formed
            && self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.digest.starts_with(digest32));
        if matches {
            self.entries.remove(&key)
        } else {
            None
        }
    }

    /// Drop the entry for `key` when one exists. Expiry and restart loss are
    /// equivalent to cancel: no audit row, no mutation.
    pub fn cancel(&mut self, key: ActorKey, now: i64) -> bool {
        self.sweep(now);
        self.entries.remove(&key).is_some()
    }

    /// Drop the entry for `key` only when it still belongs to
    /// `preview_request_id`. A redelivered attempt whose entry was superseded
    /// by a newer same-key preview (different request id, audited
    /// successfully) must unwind nothing: the newer entry keeps its row.
    pub fn cancel_owned(&mut self, key: ActorKey, preview_request_id: &str, now: i64) -> bool {
        self.sweep(now);
        let owned = self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.preview_request_id == preview_request_id);
        if owned {
            self.entries.remove(&key);
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn len(&self, now: i64) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.expires_at > now)
            .count()
    }
}

/// Sliding-window preview budget, charged after router adjudication and
/// reason validation but before any Discord read (spec section 5). Only
/// admitted previews consume budget; a refused preview costs zero GETs.
/// Restart resets the counters (damping, not accounting; the audit row is
/// the accounting).
#[derive(Debug, Default)]
pub struct RateLimiter {
    actor_hits: HashMap<u64, VecDeque<i64>>,
    guild_hits: HashMap<u64, VecDeque<i64>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(hits: &mut HashMap<u64, VecDeque<i64>>, key: u64, now: i64) {
        if let Some(times) = hits.get_mut(&key) {
            while times
                .front()
                .is_some_and(|first| *first <= now - RATE_WINDOW_SECS)
            {
                times.pop_front();
            }
            if times.is_empty() {
                hits.remove(&key);
            }
        }
    }

    /// Admit one preview for `actor` in `guild` at `now`, recording both
    /// budgets. Returns `false` (recording nothing) past either budget.
    pub fn admit(&mut self, actor: u64, guild: u64, now: i64) -> bool {
        Self::prune(&mut self.actor_hits, actor, now);
        Self::prune(&mut self.guild_hits, guild, now);
        let actor_count = self.actor_hits.get(&actor).map_or(0, VecDeque::len);
        let guild_count = self.guild_hits.get(&guild).map_or(0, VecDeque::len);
        if actor_count >= MAX_PREVIEWS_PER_ACTOR || guild_count >= MAX_PREVIEWS_PER_GUILD {
            return false;
        }
        self.actor_hits.entry(actor).or_default().push_back(now);
        self.guild_hits.entry(guild).or_default().push_back(now);
        true
    }
}

/// `lockdown-preview:confirm:<actor>:<digest32>`, at most 78 characters for
/// a 20-digit actor (spec section 3 step 3: each id stays within 100).
#[must_use]
pub fn confirm_custom_id(actor: u64, digest32: &str) -> String {
    format!("lockdown-preview:confirm:{actor}:{digest32}")
}

/// `lockdown-preview:cancel:<actor>:<digest32>`, same bound as confirm.
#[must_use]
pub fn cancel_custom_id(actor: u64, digest32: &str) -> String {
    format!("lockdown-preview:cancel:{actor}:{digest32}")
}

/// Numbers-only preview audit metadata (spec section 6). The ledger's
/// metadata discipline is bounded numbers; the confirmed-execution row adds
/// `preview_request_id` separately in the confirm slice.
#[must_use]
pub fn preview_audit_metadata(plan: &PreviewPlan) -> String {
    serde_json::json!({
        "prior_exists": u8::from(plan.prior_exists),
        "roles_evaluated": plan.roles_evaluated,
        "roles_keeping_send": plan.would_keep_send,
        "member_overwrite_allows": plan.member_overwrite_allows,
    })
    .to_string()
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Length Discord will actually receive: the interaction edit applies
/// `message_safety::content`, which inserts one zero-width space per mass
/// mention (`@everyone`/`@here`). The render carries four `@everyone`
/// tokens, so the wire form is up to four units longer than the raw text.
fn wire_len(value: &str) -> usize {
    utf16_len(&neutralize_mentions(value))
}

fn channel_kind(channel_type: u64) -> &'static str {
    if channel_type == 5 {
        "announcement"
    } else {
        "text"
    }
}

/// Fixed 11-item ephemeral render (spec section 4). The moderator's reason
/// is never echoed. When the full render would exceed the length bound, the
/// keeper names drop to a count-only line; counts and warnings are never
/// truncated, so the fallback is bounded by construction.
#[must_use]
pub fn render_preview(
    plan: &PreviewPlan,
    channel_id: u64,
    channel_type: u64,
    parent_id: Option<u64>,
    saved_present: bool,
    expires_at: i64,
) -> String {
    let category = match parent_id {
        Some(parent) => {
            format!("{parent}; sync is not evaluated, only this channel is read or written")
        }
        None => "none".to_owned(),
    };
    let everyone = if plan.prior_exists {
        format!(
            "present allow={} deny={} -> allow={} deny={}",
            plan.prior_allow, plan.prior_deny, plan.planned_allow, plan.planned_deny
        )
    } else {
        format!(
            "absent (0/0) -> created allow={} deny={}",
            plan.planned_allow, plan.planned_deny
        )
    };
    let mut bit_lines = String::new();
    for bit in &plan.bits {
        bit_lines.push_str(&format!(
            "{}: {} -> deny \u{b7} roles losing usable {}: {} \u{b7} keeping: {}\n",
            bit.name,
            bit.state.name(),
            bit.name,
            bit.losing,
            bit.keeping,
        ));
    }
    let keeper_names: Vec<String> = plan
        .keeper_role_ids
        .iter()
        .take(MAX_KEEPER_NAMES)
        .map(|id| format!("<@&{id}>"))
        .collect();
    let keeper_line = if plan.keeper_role_ids.len() > MAX_KEEPER_NAMES {
        let more = plan.keeper_role_ids.len() - MAX_KEEPER_NAMES;
        format!(
            "roles keeping SendMessages: {} (+{more} more not named; counts are complete)",
            keeper_names.join(" ")
        )
    } else if keeper_names.is_empty() {
        "roles keeping SendMessages: none".to_owned()
    } else {
        format!("roles keeping SendMessages: {}", keeper_names.join(" "))
    };
    let full = format!(
        "Lockdown preview \u{2014} nothing was changed\n\
         channel: <#{channel_id}> ({}; category {category})\n\
         expires <t:{expires_at}:R> \u{b7} Confirm re-reads live permissions and refuses if they changed\n\
         saved bot lockdown: {} \u{b7} live @everyone SendMessages: {}\n\
         @everyone overwrite: {everyone}\n\
         {bit_lines}\
         ViewChannel: unchanged by lockdown (@everyone overwrite: {})\n\
         other @everyone bits preserved: allow {} bit(s), deny {} bit(s)\n\
         projections: {} role(s) \u{b7} would lose SendMessages: {} \u{b7} would keep: {} \u{b7} cannot send already (usable): {}\n\
         {keeper_line}\n\
         member overwrites allowing a lockdown bit: {} (members are never listed; lockdown does not change these)\n\
         Administrator roles: {} (bypass channel overwrites; unchanged)\n\
         note: single-role rows are a projection, not a member roster (role allows combine, then member overwrites win)\n\
         note: the bot's write permission is not proven; Discord may still refuse the PUT; direct /lockdown stays available and ignores this preview",
        channel_kind(channel_type),
        if saved_present { "present" } else { "none" },
        plan.everyone_send_state.name(),
        plan.view_state.name(),
        plan.preserved_allow_bits,
        plan.preserved_deny_bits,
        plan.roles_evaluated,
        plan.would_lose_send,
        plan.would_keep_send,
        plan.cannot_send_already,
        plan.member_overwrite_allows,
        plan.administrator_roles,
    );
    // Measure the neutralized wire form, not the raw text: the four
    // `@everyone` tokens each gain one unit on the way out. The keeper
    // names the fallback drops are never mass mentions, so the fallback is
    // always strictly shorter on the wire than the full render it replaces.
    if wire_len(&full) <= MAX_RENDER_UTF16 {
        return full;
    }
    let count_only = format!(
        "roles keeping SendMessages: {} role(s) (names withheld to fit the length bound; counts are complete)",
        plan.keeper_role_ids.len()
    );
    full.replacen(&keeper_line, &count_only, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::lockdown_preview::{OverwriteSnapshot, PreviewSnapshot, RoleSnapshot};

    const VIEW: u64 = 1 << 10;
    const SEND: u64 = 1 << 11;
    const ADMINISTRATOR: u64 = 1 << 3;

    fn snapshot() -> PreviewSnapshot {
        PreviewSnapshot {
            guild_id: 10,
            channel_id: 20,
            channel_type: 0,
            parent_id: Some(40),
            actor_id: 30,
            reason: "raid".to_owned(),
            saved_present: false,
            saved_generation: String::new(),
            roles: vec![
                RoleSnapshot {
                    id: 10,
                    position: 0,
                    permissions: VIEW | SEND,
                },
                RoleSnapshot {
                    id: 50,
                    position: 5,
                    permissions: 0,
                },
            ],
            overwrites: vec![OverwriteSnapshot {
                kind: 0,
                id: 10,
                allow: 0,
                deny: 0,
            }],
        }
    }

    fn pending(digest: &str, expires_at: i64) -> PendingEntry {
        PendingEntry {
            digest: digest.to_owned(),
            reason: "raid".to_owned(),
            preview_request_id: "99".to_owned(),
            expires_at,
        }
    }

    #[test]
    fn replacement_at_cap_succeeds_and_superseded_digest_is_dead() {
        let mut store = PendingStore::new();
        for actor in 0..64 {
            store
                .insert((1, 2, actor), pending(&format!("{actor:064}"), 300), 1)
                .expect("fills the cap");
        }
        assert_eq!(store.len(1), 64);
        store
            .insert((1, 2, 9), pending(&"9".repeat(64), 301), 1)
            .expect("same-key replacement succeeds at cap");
        assert_eq!(store.len(1), 64);
        // A new key at cap refuses and keeps every live entry.
        assert_eq!(
            store.insert((1, 2, 64), pending(&"4".repeat(64), 301), 1),
            Err(PendingFull)
        );
        assert_eq!(store.len(1), 64);
        // The superseded digest no longer consumes; the consume is separate
        // from the at-cap refusal above.
        assert!(store.take((1, 2, 9), &"0".repeat(32), 1).is_none());
        assert!(store.take((1, 2, 9), &"9".repeat(32), 1).is_some());
        assert_eq!(store.len(1), 63);
    }

    #[test]
    fn expiry_equality_frees_and_take_refuses_expired() {
        let mut store = PendingStore::new();
        store
            .insert((1, 2, 3), pending(&"a".repeat(64), 300), 1)
            .expect("inserts");
        // now == expires is expired: take refuses (LP-11) and the slot frees.
        assert!(store.take((1, 2, 3), &"a".repeat(32), 300).is_none());
        assert_eq!(store.len(300), 0);
        store
            .insert((1, 2, 4), pending(&"b".repeat(64), 301), 300)
            .expect("expired slot is reusable");
        assert!(store.take((1, 2, 4), &"b".repeat(32), 300).is_some());
        // Double press and foreign digest both refuse.
        store
            .insert((1, 2, 5), pending(&"c".repeat(64), 400), 300)
            .expect("inserts");
        assert!(store.take((1, 2, 5), &"c".repeat(32), 301).is_some());
        assert!(store.take((1, 2, 5), &"c".repeat(32), 302).is_none());
    }

    #[test]
    fn cancel_removes_without_audit_trace() {
        let mut store = PendingStore::new();
        store
            .insert((1, 2, 3), pending(&"a".repeat(64), 300), 1)
            .expect("inserts");
        assert!(store.cancel((1, 2, 3), 2));
        assert!(!store.cancel((1, 2, 3), 2));
        assert_eq!(store.len(2), 0);
    }

    fn owned_entry(digest_char: char, request_id: &str) -> PendingEntry {
        PendingEntry {
            digest: digest_char.to_string().repeat(64),
            reason: "raid".to_owned(),
            preview_request_id: request_id.to_owned(),
            expires_at: 300,
        }
    }

    #[test]
    fn take_rejects_malformed_keys_without_consuming() {
        let mut store = PendingStore::new();
        store
            .insert((1, 2, 3), owned_entry('a', "7"), 1)
            .expect("inserts");
        // Short, long, empty and non-hex prefixes all refuse.
        for malformed in [
            "a".repeat(31),
            "a".repeat(33),
            String::new(),
            "g".repeat(32),
            "A".repeat(31) + "!",
        ] {
            assert!(
                store.take((1, 2, 3), &malformed, 2).is_none(),
                "malformed key must not consume: {malformed:?}"
            );
        }
        assert_eq!(store.len(2), 1, "refusals consume nothing");
        assert!(store.take((1, 2, 3), &"a".repeat(32), 2).is_some());
    }

    #[test]
    fn cancel_owned_spares_a_superseding_preview() {
        let mut store = PendingStore::new();
        let key = (1, 2, 3);
        store
            .insert(key, owned_entry('a', "111"), 1)
            .expect("inserts");
        // A newer same-key preview supersedes the redelivered attempt's entry.
        store
            .insert(key, owned_entry('a', "222"), 2)
            .expect("replaces");
        // The stale attempt's unwind must not delete the newer entry.
        assert!(!store.cancel_owned(key, "111", 3));
        assert_eq!(store.len(3), 1);
        assert!(store.take(key, &"a".repeat(32), 3).is_some());
        // An owner-matched unwind removes its own entry.
        store
            .insert(key, owned_entry('b', "333"), 4)
            .expect("inserts");
        assert!(store.cancel_owned(key, "333", 5));
        assert_eq!(store.len(5), 0);
    }

    #[test]
    fn rate_limiter_budgets_actor_and_guild() {
        let mut limiter = RateLimiter::new();
        for _ in 0..10 {
            assert!(limiter.admit(7, 9, 0));
        }
        // 11th preview by one actor inside 10 min refuses (LP-15).
        assert!(!limiter.admit(7, 9, 1));
        // A different actor in the same guild still passes until the guild cap.
        for actor in 100..120 {
            assert!(limiter.admit(actor, 9, 2));
        }
        assert!(!limiter.admit(999, 9, 3));
        // The window slides: the first hits expire 600 s later.
        assert!(limiter.admit(7, 8, 600));
        assert!(limiter.admit(999, 9, 602));
    }

    #[test]
    fn custom_ids_stay_within_the_button_bound() {
        let digest32 = "a".repeat(32);
        let confirm = confirm_custom_id(18_446_744_073_709_551_615, &digest32);
        let cancel = cancel_custom_id(18_446_744_073_709_551_615, &digest32);
        assert!(confirm.len() <= 100);
        assert!(cancel.len() <= 100);
        assert!(confirm.starts_with("lockdown-preview:confirm:"));
        assert!(cancel.starts_with("lockdown-preview:cancel:"));
    }

    #[test]
    fn audit_metadata_is_numbers_only() {
        let plan = two_bot_core::lockdown_preview::plan_preview(&snapshot()).expect("plans");
        let metadata: serde_json::Value =
            serde_json::from_str(&preview_audit_metadata(&plan)).expect("valid json");
        for (key, value) in metadata.as_object().expect("object") {
            assert!(value.is_number(), "{key} must be a number, never text");
        }
        assert_eq!(metadata["roles_evaluated"], 2);
    }

    #[test]
    fn render_states_every_fixed_line_and_fits() {
        let plan = two_bot_core::lockdown_preview::plan_preview(&snapshot()).expect("plans");
        assert!(plan.armed);
        let text = render_preview(&plan, 20, 0, Some(40), false, 1_800_000_300);
        for line in [
            "Lockdown preview \u{2014} nothing was changed",
            "channel: <#20> (text; category 40",
            "sync is not evaluated, only this channel is read or written",
            "expires <t:1800000300:R>",
            "saved bot lockdown: none",
            "live @everyone SendMessages:",
            "@everyone overwrite: present allow=0 deny=0",
            "SendMessages: neutral -> deny",
            "ViewChannel: unchanged by lockdown",
            "other @everyone bits preserved:",
            "projections: 2 role(s)",
            "roles keeping SendMessages:",
            "member overwrites allowing a lockdown bit: 0",
            "Administrator roles: 0",
            "not a member roster",
            "direct /lockdown stays available",
        ] {
            assert!(text.contains(line), "render misses {line:?}:\n{text}");
        }
        assert!(!text.contains("raid"), "the reason is never echoed");
        assert!(utf16_len(&text) <= MAX_RENDER_UTF16);
    }

    #[test]
    fn render_names_keepers_position_first_and_truncates_to_counts() {
        let mut snap = snapshot();
        snap.roles.extend((100..112).map(|id| RoleSnapshot {
            id,
            position: id as i64,
            permissions: 0,
        }));
        // Each role's own overwrite allows SEND, so all 12 keep usable send
        // after the lockdown (the two base roles lose it); 11 named keepers
        // plus one must truncate.
        snap.overwrites
            .extend((100..112).map(|id| OverwriteSnapshot {
                kind: 0,
                id,
                allow: SEND,
                deny: 0,
            }));
        let plan = two_bot_core::lockdown_preview::plan_preview(&snap).expect("plans");
        assert_eq!(plan.keeper_role_ids.len(), 12);
        let text = render_preview(&plan, 20, 5, None, true, 1_800_000_300);
        assert!(text.contains("(announcement; category none)"));
        assert!(text.contains("saved bot lockdown: present"));
        assert!(text.contains("+2 more not named; counts are complete"));
        assert!(utf16_len(&text) <= MAX_RENDER_UTF16);
        // Count-only fallback keeps every count intact under the bound.
        let count_only = text.replacen(
            &format!(
                "roles keeping SendMessages: {} (+2 more not named; counts are complete)",
                plan.keeper_role_ids
                    .iter()
                    .take(MAX_KEEPER_NAMES)
                    .map(|id| format!("<@&{id}>"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "roles keeping SendMessages: 12 role(s) (names withheld to fit the length bound; counts are complete)",
            1,
        );
        assert!(utf16_len(&count_only) <= MAX_RENDER_UTF16);
        assert!(count_only.contains("would keep: 12"));
    }

    /// Adversarial snapshot: 250 evaluated roles, maximum-width ids and
    /// masks, saved state present. Keeper roles carry Administrator so they
    /// keep usable SendMessages without one overwrite each (the snapshot
    /// overwrite cap is 100), and the `@everyone` overwrite carries full
    /// masks for maximum-width allow/deny text. Without keepers the render
    /// would stay small and never exercise the count-only fallback. The
    /// interaction edit neutralizes the four `@everyone` tokens (+1 unit
    /// each), so the bound applies to the wire form, not the raw render.
    fn adversarial_snapshot(keepers: usize, id_base: u64) -> PreviewSnapshot {
        let mut roles = vec![RoleSnapshot {
            id: 7,
            position: 0,
            permissions: VIEW,
        }];
        roles.extend((0..keepers).map(|index| RoleSnapshot {
            id: id_base + index as u64,
            position: index as i64 + 1,
            permissions: VIEW | SEND | ADMINISTRATOR,
        }));
        roles.extend((keepers..249).map(|index| RoleSnapshot {
            id: 1_000_000 + index as u64,
            position: index as i64 + 1,
            permissions: 0,
        }));
        PreviewSnapshot {
            guild_id: 7,
            channel_id: u64::MAX,
            channel_type: 5,
            parent_id: Some(u64::MAX),
            actor_id: 9,
            reason: "raid".to_owned(),
            saved_present: true,
            saved_generation: "3".to_owned(),
            roles,
            overwrites: vec![OverwriteSnapshot {
                kind: 0,
                id: 7,
                allow: u64::MAX,
                deny: u64::MAX,
            }],
        }
    }

    #[test]
    fn render_wire_form_after_neutralization_fits_the_bound() {
        // Maximum width everywhere: 20-digit ids, u64 masks, 249 keepers.
        let widest = adversarial_snapshot(249, 10_000_000_000_000_000_000);
        let plan = two_bot_core::lockdown_preview::plan_preview(&widest).expect("plans");
        assert!(plan.armed);
        let text = render_preview(&plan, u64::MAX, 5, Some(u64::MAX), true, 1_800_000_300);
        assert!(
            text.contains("names withheld"),
            "the widest shape must take the count-only fallback"
        );
        assert!(
            wire_len(&text) <= MAX_RENDER_UTF16,
            "wire form exceeds the bound: {} units",
            wire_len(&text)
        );
        // Truncation edge and narrow shapes stay on the wire bound too.
        for keepers in [0, 1, 10, 11] {
            let snap = adversarial_snapshot(keepers, 999_999_999_999_999_999);
            let plan = two_bot_core::lockdown_preview::plan_preview(&snap).expect("plans");
            let text = render_preview(&plan, 3_333, 0, None, false, 1_800_000_300);
            assert!(
                wire_len(&text) <= MAX_RENDER_UTF16,
                "{keepers} keepers wire to {} units",
                wire_len(&text)
            );
        }
    }
}
