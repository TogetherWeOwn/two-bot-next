//! Referral rewards: experiential/status rewards, 2nd-hop bonus, contagion framing.
//!
//! Consumes the invite tracker's attribution outputs. Only an `exact`
//! `invite:CODE` attribution credits an individual referrer: aggregate
//! (`ambiguous:*`), `vanity` and `unknown` sources describe counts, never
//! proof of who invited whom, so they can never mint credit. The code →
//! inviter map mirrors the `invite_snapshots` shape (`code`, `inviter_id`).
//!
//! Rewards are experiential/status only (spotlight, cosmetic, role). The type
//! system has no cash-equivalent kind, so one cannot be added by accident.
//!
//! 2nd-hop bonus: when a credited join's inviter was themselves referred (a
//! prior credited edge, or one credited in the same batch), the
//! grand-inviter earns a bonus credit. The bonus never pays the inviter
//! themselves or the member who just joined. Referred members get
//! `referred-in, now refer` framing copy, which is the contagion lever:
//! referred users refer downstream at higher rates.
//!
//! Anti-fraud: self-referrals, duplicate joins (leave-rejoin farming) and
//! temporary memberships (joined then left before the minimum tenure) are
//! rejected; young-or-quiet memberships stay pending and re-evaluable, never
//! credited early. First attribution wins.
//!
//! Pure: data in, report out. Wiring sources into [`ReferralInput`] and
//! delivering [`Notice`]s stays with the caller.

use std::collections::{HashMap, HashSet};

use crate::Snowflake;

/// Default minimum genuine-membership tenure before a referral credits.
pub const MIN_TENURE_DAYS_DEFAULT: i64 = 7;

/// Credit totals that unlock each experiential tier.
pub const TIER_SPOTLIGHT_MIN: u64 = 1;
pub const TIER_COSMETIC_MIN: u64 = 3;
pub const TIER_ROLE_MIN: u64 = 5;

/// One join as attributed by the invite tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributedJoin {
    pub member_id: Snowflake,
    /// Attribution source string (`invite:CODE`, `ambiguous:a+b`, `vanity`, …).
    pub source: String,
    /// True only when this member provably came through this code.
    pub exact: bool,
    pub joined_at_ms: i64,
}

/// What the referred member has done since joining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferralStanding {
    pub left_at_ms: Option<i64>,
    /// Human activity signal (message, voice, rules accepted…); the caller
    /// wires community facts here in a later slice.
    pub engaged: bool,
}

/// Everything the evaluator needs for one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferralInput {
    pub joins: Vec<AttributedJoin>,
    pub code_inviter: HashMap<String, Snowflake>,
    /// Prior credited edges (member → inviter), all earlier batches plus
    /// already-merged history. Drives 2nd-hop lookup and rejoin dedupe.
    pub known_referrals: HashMap<Snowflake, Snowflake>,
    /// Per-inviter credit totals banked in earlier batches (direct + bonus as
    /// counted toward tiers). Tiers accumulate across batches: the reported
    /// tier covers prior plus this batch, and a milestone fires only when the
    /// batch lifts the inviter above its pre-batch tier.
    pub prior_credits: HashMap<Snowflake, u64>,
    pub standings: HashMap<Snowflake, ReferralStanding>,
    pub now_ms: i64,
    pub min_tenure_days: i64,
}

/// Why a join waits instead of crediting. Re-runnable later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingReason {
    TenureTooShort,
    AwaitingEngagement,
}

/// Why a join can never credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// Aggregate, vanity or unknown attribution: counts, not proof.
    NotProven,
    /// Exact `invite:CODE` with no known inviter for the code.
    UnknownCode,
    SelfReferral,
    /// Same member already credited (leave-rejoin farming guard).
    DuplicateJoin,
    /// Joined then left before the minimum tenure.
    TemporaryMembership,
}

/// One join's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOutcome {
    Credited {
        inviter_id: Snowflake,
        second_hop: Option<Snowflake>,
    },
    Pending(PendingReason),
    Rejected(RejectionReason),
}

/// One join plus its verdict, in input order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinVerdict {
    pub member_id: Snowflake,
    pub outcome: JoinOutcome,
}

/// Experiential reward kinds. Deliberately closed: no cash equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewardKind {
    Spotlight,
    Cosmetic,
    Role,
}

/// One unlocked tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewardTier {
    pub kind: RewardKind,
    pub title: &'static str,
}

/// One deliverable line: who gets it and the contagion-framed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub recipient_id: Snowflake,
    pub text: String,
}

/// Full batch outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferralReport {
    pub verdicts: Vec<JoinVerdict>,
    pub direct_credits: HashMap<Snowflake, u64>,
    pub bonus_credits: HashMap<Snowflake, u64>,
    pub tiers: HashMap<Snowflake, RewardTier>,
    pub notices: Vec<Notice>,
}

/// The invite code inside an attribution source, if it names exactly one.
#[must_use]
pub fn invite_code(source: &str) -> Option<&str> {
    source.strip_prefix("invite:")
}

/// Highest tier unlocked by a credit total (direct + bonus).
#[must_use]
pub fn tier_for_total(total: u64) -> Option<RewardTier> {
    if total >= TIER_ROLE_MIN {
        Some(RewardTier {
            kind: RewardKind::Role,
            title: "Referrer status role",
        })
    } else if total >= TIER_COSMETIC_MIN {
        Some(RewardTier {
            kind: RewardKind::Cosmetic,
            title: "Profile flair",
        })
    } else if total >= TIER_SPOTLIGHT_MIN {
        Some(RewardTier {
            kind: RewardKind::Spotlight,
            title: "Member spotlight",
        })
    } else {
        None
    }
}

/// Referred-in framing for the referred member: named inviter, immediate
/// call to refer. The contagion lever.
#[must_use]
pub fn referred_in_copy(inviter_id: Snowflake) -> String {
    format!(
        "You were referred in by member {inviter_id} - now refer someone in! \
         Your first referral earns a member spotlight."
    )
}

/// Milestone copy for an inviter who unlocked a tier.
#[must_use]
pub fn tier_copy(tier: RewardTier) -> String {
    let perk = match tier.kind {
        RewardKind::Spotlight => "a member spotlight shout-out",
        RewardKind::Cosmetic => "a profile flair cosmetic",
        RewardKind::Role => "a referrer status role",
    };
    format!(
        "Referral milestone unlocked: {}! Your invites earned {perk}.",
        tier.title
    )
}

/// 2nd-hop bonus copy for the grand-inviter.
#[must_use]
pub fn second_hop_copy(referred_inviter: Snowflake) -> String {
    format!(
        "Someone you referred (member {referred_inviter}) just referred another \
         member in - 2nd-hop bonus credit earned! Referred members refer more."
    )
}

/// Evaluate one batch of attributed joins into credits, tiers and notices.
///
/// Second-hop bonuses resolve against prior edges plus every edge credited
/// in this batch, so chains that mature together still pay the bonus and
/// input order does not matter. Tier milestones compare against
/// [`ReferralInput::prior_credits`], so a milestone fires only when this
/// batch lifts an inviter above its pre-batch tier.
///
/// Notices come out in a stable order: per-join framing in input order, then
/// one tier milestone per newly-unlocked inviter ordered by inviter id.
#[must_use]
pub fn evaluate_referrals(input: &ReferralInput) -> ReferralReport {
    let mut verdicts = Vec::with_capacity(input.joins.len());
    let mut seen: HashSet<Snowflake> = HashSet::new();

    // Credit pass: verdicts without second-hop yet; collect this batch's edges.
    let mut batch_edges: Vec<(Snowflake, Snowflake)> = Vec::new();
    for join in &input.joins {
        let member = join.member_id;
        let already_seen = !seen.insert(member);
        let outcome = evaluate_one(input, join, already_seen);
        if let JoinOutcome::Credited { inviter_id, .. } = outcome {
            batch_edges.push((member, inviter_id));
        }
        verdicts.push(JoinVerdict {
            member_id: member,
            outcome,
        });
    }

    // Resolution pass: the working edge map is prior edges plus everything
    // credited above, so a parent credited in this batch is visible to its
    // children regardless of input order. A bonus never pays the inviter
    // themselves, nor the member who just joined.
    let mut edges = input.known_referrals.clone();
    for (member, inviter) in &batch_edges {
        edges.insert(*member, *inviter);
    }
    for verdict in &mut verdicts {
        if let JoinOutcome::Credited {
            inviter_id,
            second_hop,
        } = &mut verdict.outcome
        {
            *second_hop = edges
                .get(inviter_id)
                .copied()
                .filter(|grand| *grand != *inviter_id && *grand != verdict.member_id);
        }
    }

    let mut direct_credits: HashMap<Snowflake, u64> = HashMap::new();
    let mut bonus_credits: HashMap<Snowflake, u64> = HashMap::new();
    let mut notices = Vec::new();
    for verdict in &verdicts {
        if let JoinOutcome::Credited {
            inviter_id,
            second_hop,
        } = verdict.outcome
        {
            *direct_credits.entry(inviter_id).or_default() += 1;
            notices.push(Notice {
                recipient_id: verdict.member_id,
                text: referred_in_copy(inviter_id),
            });
            if let Some(grand) = second_hop {
                *bonus_credits.entry(grand).or_default() += 1;
                notices.push(Notice {
                    recipient_id: grand,
                    text: second_hop_copy(inviter_id),
                });
            }
        }
    }

    // Tiers accumulate: prior totals plus this batch. A milestone fires only
    // when the batch lifts the inviter above its pre-batch tier, so repeat
    // credits at the same tier do not re-notify.
    let mut tiers = HashMap::new();
    let mut inviters: Vec<Snowflake> = direct_credits
        .keys()
        .chain(bonus_credits.keys())
        .copied()
        .collect();
    inviters.sort_unstable();
    inviters.dedup();
    for inviter in inviters {
        let batch_total = direct_credits.get(&inviter).copied().unwrap_or(0)
            + bonus_credits.get(&inviter).copied().unwrap_or(0);
        let prior = input.prior_credits.get(&inviter).copied().unwrap_or(0);
        if let Some(tier) = tier_for_total(prior + batch_total) {
            let is_new_unlock = tier_for_total(prior).is_none_or(|before| before.kind != tier.kind);
            if is_new_unlock {
                notices.push(Notice {
                    recipient_id: inviter,
                    text: tier_copy(tier),
                });
            }
            tiers.insert(inviter, tier);
        }
    }

    ReferralReport {
        verdicts,
        direct_credits,
        bonus_credits,
        tiers,
        notices,
    }
}

/// Verdict for a single join. `already_seen` marks a repeat of the same
/// member earlier in this batch (leave-rejoin farming guard). A credited join
/// comes back with `second_hop: None`; the caller resolves it once the whole
/// batch's edges are known.
fn evaluate_one(input: &ReferralInput, join: &AttributedJoin, already_seen: bool) -> JoinOutcome {
    if already_seen || input.known_referrals.contains_key(&join.member_id) {
        return JoinOutcome::Rejected(RejectionReason::DuplicateJoin);
    }
    if !join.exact {
        return JoinOutcome::Rejected(RejectionReason::NotProven);
    }
    let code = match invite_code(&join.source) {
        Some(code) => code,
        None => return JoinOutcome::Rejected(RejectionReason::NotProven),
    };
    let inviter = match input.code_inviter.get(code) {
        Some(inviter) => *inviter,
        None => return JoinOutcome::Rejected(RejectionReason::UnknownCode),
    };
    if inviter == join.member_id {
        return JoinOutcome::Rejected(RejectionReason::SelfReferral);
    }
    let standing = input.standings.get(&join.member_id);
    let left_at = standing.and_then(|s| s.left_at_ms);
    let engaged = standing.is_some_and(|s| s.engaged);
    let tenure = crate::onboarding::days_in_guild(
        Some(join.joined_at_ms),
        Some(left_at.unwrap_or(input.now_ms)),
    )
    .unwrap_or(0);
    if tenure < input.min_tenure_days {
        return if left_at.is_some() {
            JoinOutcome::Rejected(RejectionReason::TemporaryMembership)
        } else {
            JoinOutcome::Pending(PendingReason::TenureTooShort)
        };
    }
    if !engaged {
        return JoinOutcome::Pending(PendingReason::AwaitingEngagement);
    }
    // Second-hop resolves later in `evaluate_referrals`, against prior edges
    // plus every edge credited in this batch.
    JoinOutcome::Credited {
        inviter_id: inviter,
        second_hop: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn join(member: Snowflake, source: &str, exact: bool, joined_ago_days: i64) -> AttributedJoin {
        AttributedJoin {
            member_id: member,
            source: source.to_owned(),
            exact,
            joined_at_ms: 1_000_000_000_000 - joined_ago_days * DAY,
        }
    }

    fn present(engaged: bool) -> ReferralStanding {
        ReferralStanding {
            left_at_ms: None,
            engaged,
        }
    }

    fn input(
        joins: Vec<AttributedJoin>,
        codes: &[(&str, Snowflake)],
        edges: &[(Snowflake, Snowflake)],
        standings: Vec<(Snowflake, ReferralStanding)>,
    ) -> ReferralInput {
        ReferralInput {
            joins,
            code_inviter: codes.iter().map(|(c, id)| ((*c).to_owned(), *id)).collect(),
            known_referrals: edges.iter().copied().collect(),
            prior_credits: HashMap::new(),
            standings: standings.into_iter().collect(),
            now_ms: 1_000_000_000_000,
            min_tenure_days: MIN_TENURE_DAYS_DEFAULT,
        }
    }

    fn input_prior(
        joins: Vec<AttributedJoin>,
        codes: &[(&str, Snowflake)],
        edges: &[(Snowflake, Snowflake)],
        standings: Vec<(Snowflake, ReferralStanding)>,
        prior: &[(Snowflake, u64)],
    ) -> ReferralInput {
        let mut base = input(joins, codes, edges, standings);
        base.prior_credits = prior.iter().copied().collect();
        base
    }

    fn credited(
        report: &ReferralReport,
        member: Snowflake,
    ) -> Option<(Snowflake, Option<Snowflake>)> {
        report
            .verdicts
            .iter()
            .find(|v| v.member_id == member)
            .and_then(|v| match v.outcome {
                JoinOutcome::Credited {
                    inviter_id,
                    second_hop,
                } => Some((inviter_id, second_hop)),
                _ => None,
            })
    }

    #[test]
    fn source_parsing_names_exactly_one_code() {
        assert_eq!(invite_code("invite:abc123"), Some("abc123"));
        assert_eq!(invite_code("ambiguous:a+b"), None);
        assert_eq!(invite_code("vanity"), None);
        assert_eq!(invite_code("unknown"), None);
        assert_eq!(invite_code("web:one_click"), None);
    }

    #[test]
    fn exact_join_with_known_code_credits_inviter() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(credited(&report, 2), Some((1, None)));
        assert_eq!(report.direct_credits.get(&1), Some(&1));
    }

    #[test]
    fn aggregate_attribution_never_credits_an_individual() {
        let report = evaluate_referrals(&input(
            vec![join(2, "ambiguous:aaa+bbb", false, 10)],
            &[("aaa", 1), ("bbb", 3)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::NotProven)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn inexact_single_code_is_still_not_proof() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", false, 10)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::NotProven)
        );
    }

    #[test]
    fn exact_code_with_unknown_inviter_is_rejected() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:zzz", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::UnknownCode)
        );
    }

    #[test]
    fn self_referral_is_rejected() {
        let report = evaluate_referrals(&input(
            vec![join(1, "invite:aaa", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(1, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::SelfReferral)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn leave_rejoin_farming_credits_only_the_first_join() {
        let old = join(2, "invite:aaa", true, 30);
        let rejoin = AttributedJoin {
            joined_at_ms: 1_000_000_000_000 - DAY,
            ..old.clone()
        };
        let report = evaluate_referrals(&input(
            vec![old, rejoin],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Credited {
                inviter_id: 1,
                second_hop: None,
            }
        );
        assert_eq!(
            report.verdicts[1].outcome,
            JoinOutcome::Rejected(RejectionReason::DuplicateJoin)
        );
        assert_eq!(report.direct_credits.get(&1), Some(&1));
    }

    #[test]
    fn already_referred_member_rejoining_is_rejected() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:bbb", true, 10)],
            &[("bbb", 3)],
            &[(2, 1)],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::DuplicateJoin)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn young_membership_stays_pending_not_credited() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 2)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Pending(PendingReason::TenureTooShort)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn quiet_membership_stays_pending_despite_tenure() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 30)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(false))],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Pending(PendingReason::AwaitingEngagement)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn temporary_membership_is_rejected() {
        let standing = ReferralStanding {
            left_at_ms: Some(1_000_000_000_000 - 29 * DAY),
            engaged: true,
        };
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 30)],
            &[("aaa", 1)],
            &[],
            vec![(2, standing)],
        ));
        assert_eq!(
            report.verdicts[0].outcome,
            JoinOutcome::Rejected(RejectionReason::TemporaryMembership)
        );
        assert!(report.direct_credits.is_empty());
    }

    #[test]
    fn genuine_stay_who_later_left_still_credits() {
        let standing = ReferralStanding {
            left_at_ms: Some(1_000_000_000_000 - 5 * DAY),
            engaged: true,
        };
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 30)],
            &[("aaa", 1)],
            &[],
            vec![(2, standing)],
        ));
        assert_eq!(credited(&report, 2), Some((1, None)));
    }

    #[test]
    fn second_hop_bonus_flows_to_grand_inviter() {
        // Member 1 referred member 2 before this batch; member 3 joins
        // through member 2's code: 2 earns direct, 1 earns the bonus.
        let report = evaluate_referrals(&input(
            vec![join(3, "invite:bbb", true, 10)],
            &[("bbb", 2)],
            &[(2, 1)],
            vec![(3, present(true))],
        ));
        assert_eq!(credited(&report, 3), Some((2, Some(1))));
        assert_eq!(report.direct_credits.get(&2), Some(&1));
        assert_eq!(report.bonus_credits.get(&1), Some(&1));
    }

    #[test]
    fn tiers_are_experiential_and_unlock_in_order() {
        assert_eq!(tier_for_total(0), None);
        let one = tier_for_total(1).expect("first credit spotlights");
        let three = tier_for_total(3).expect("third credit unlocks cosmetic");
        let five = tier_for_total(5).expect("fifth credit unlocks role");
        assert_eq!(one.kind, RewardKind::Spotlight);
        assert_eq!(three.kind, RewardKind::Cosmetic);
        assert_eq!(five.kind, RewardKind::Role);
        // Totals, not per-kind counters: bonus and direct mix freely.
        assert_eq!(
            tier_for_total(4).map(|t| t.kind),
            Some(RewardKind::Cosmetic)
        );
        assert_eq!(tier_for_total(9).map(|t| t.kind), Some(RewardKind::Role));
    }

    #[test]
    fn report_tallies_totals_and_unlocks_tiers() {
        let joins: Vec<AttributedJoin> =
            (10..15).map(|m| join(m, "invite:aaa", true, 10)).collect();
        let standings: Vec<(Snowflake, ReferralStanding)> =
            (10..15).map(|m| (m, present(true))).collect();
        let report = evaluate_referrals(&input(joins, &[("aaa", 1)], &[], standings));
        assert_eq!(report.direct_credits.get(&1), Some(&5));
        assert_eq!(report.tiers.get(&1).map(|t| t.kind), Some(RewardKind::Role));
        assert!(
            report
                .notices
                .iter()
                .any(|n| n.recipient_id == 1 && n.text.contains("status role")),
            "inviter unlocks a tier notice, got: {:?}",
            report.notices
        );
    }

    #[test]
    fn referred_member_gets_referred_in_framing() {
        let report = evaluate_referrals(&input(
            vec![join(2, "invite:aaa", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(2, present(true))],
        ));
        let framing = report
            .notices
            .iter()
            .find(|n| n.recipient_id == 2)
            .expect("referred member gets a notice");
        assert!(
            framing.text.contains("referred in") && framing.text.contains("refer"),
            "contagion framing names the referred-in state and the ask, got: {:?}",
            framing.text
        );
        let copy = referred_in_copy(1);
        assert!(copy.contains("referred in") && copy.contains("refer"));
    }

    #[test]
    fn second_hop_notice_reaches_grand_inviter() {
        let report = evaluate_referrals(&input(
            vec![join(3, "invite:bbb", true, 10)],
            &[("bbb", 2)],
            &[(2, 1)],
            vec![(3, present(true))],
        ));
        assert!(
            report
                .notices
                .iter()
                .any(|n| n.recipient_id == 1 && n.text.contains("bonus")),
            "grand-inviter hears about the 2nd-hop bonus, got: {:?}",
            report.notices
        );
        assert!(second_hop_copy(2).contains("bonus"));
    }

    #[test]
    fn rejected_and_pending_joins_emit_no_notices() {
        let report = evaluate_referrals(&input(
            vec![
                join(2, "ambiguous:aaa+bbb", false, 10),
                join(3, "invite:aaa", true, 1),
            ],
            &[("aaa", 1), ("bbb", 1)],
            &[],
            vec![(2, present(true)), (3, present(true))],
        ));
        assert!(report.notices.is_empty());
        assert_eq!(report.verdicts.len(), 2);
    }

    #[test]
    fn tiers_accumulate_across_batches_and_notify_once_per_tier() {
        // Inviter 1 banked 4 credits before this batch; one more reaches the
        // 5-credit role tier instead of repeating the spotlight milestone.
        let report = evaluate_referrals(&input_prior(
            vec![join(20, "invite:aaa", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(20, present(true))],
            &[(1, 4)],
        ));
        assert_eq!(report.direct_credits.get(&1), Some(&1));
        assert_eq!(report.tiers.get(&1).map(|t| t.kind), Some(RewardKind::Role));
        assert!(
            report
                .notices
                .iter()
                .any(|n| n.recipient_id == 1 && n.text.contains("status role")),
            "fifth cumulative credit unlocks the role milestone, got: {:?}",
            report.notices
        );
        assert!(
            report
                .notices
                .iter()
                .all(|n| { n.recipient_id != 1 || !n.text.contains("spotlight") }),
            "no repeat spotlight milestone, got: {:?}",
            report.notices
        );

        // A repeat credit inside the same tier re-notifies nothing: prior 1
        // plus one more is still spotlight, so inviter 1 gets no notice.
        let report = evaluate_referrals(&input_prior(
            vec![join(21, "invite:aaa", true, 10)],
            &[("aaa", 1)],
            &[],
            vec![(21, present(true))],
            &[(1, 1)],
        ));
        assert_eq!(
            report.tiers.get(&1).map(|t| t.kind),
            Some(RewardKind::Spotlight)
        );
        assert!(
            report.notices.iter().all(|n| n.recipient_id != 1),
            "same-tier repeat credit must not re-notify, got: {:?}",
            report.notices
        );
    }

    #[test]
    fn second_hop_bonus_is_never_paid_to_the_joining_member() {
        // Edge 2->1 exists; member 1 rejoins through 2's code. The direct
        // credit stands, but member 1 must not earn a bonus (or a milestone)
        // for its own join.
        let report = evaluate_referrals(&input(
            vec![join(1, "invite:bbb", true, 10)],
            &[("bbb", 2)],
            &[(2, 1)],
            vec![(1, present(true))],
        ));
        assert_eq!(credited(&report, 1), Some((2, None)));
        assert!(report.bonus_credits.is_empty());
        assert!(
            report.notices.iter().all(|n| !n.text.contains("bonus")),
            "no bonus notice may fire, got: {:?}",
            report.notices
        );
    }

    #[test]
    fn second_hop_bonus_survives_same_batch_chains_in_any_order() {
        // 1->2 and 2->3 mature in one batch: member 3 still pays the bonus
        // to 1, whichever order the joins arrive in.
        for joins in [
            vec![
                join(2, "invite:aaa", true, 10),
                join(3, "invite:bbb", true, 10),
            ],
            vec![
                join(3, "invite:bbb", true, 10),
                join(2, "invite:aaa", true, 10),
            ],
        ] {
            let standings = vec![(2, present(true)), (3, present(true))];
            let report =
                evaluate_referrals(&input(joins, &[("aaa", 1), ("bbb", 2)], &[], standings));
            assert_eq!(credited(&report, 2), Some((1, None)));
            assert_eq!(credited(&report, 3), Some((2, Some(1))));
            assert_eq!(report.bonus_credits.get(&1), Some(&1));
        }
    }
}
