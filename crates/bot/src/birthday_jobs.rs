//! One supervised UTC-day sweep. No detached effects; durable intents survive
//! cancellation. Uncertain cheers are fenced, never retried by age or restart.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{collections::HashSet, sync::Arc, time::Duration};
use time::{Date, Month, OffsetDateTime};
use two_bot_core::activation::LiveCapability;
use two_bot_discord::ActionExecutor;

use crate::{
    activation::BootActivation,
    birthday_command::{render, safe_channel, safe_role},
    birthday_store as store,
    jobs::{ErrorClass, Job},
    member_runtime::MemberRuntime,
    website_jobs::Context,
};

pub(crate) const NAME: &str = "birthdays";

pub(crate) fn register(context: Arc<Context>, activation: &BootActivation) -> Option<Job> {
    // Announcements is staging-only in the activation allowlist. Do not add a
    // new production clearance for this slice. The live gate is re-read per tick.
    if !activation.permitted(LiveCapability::Announcements) {
        return None;
    }
    Some(Job {
        name: NAME,
        cadence: Duration::from_secs(300),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(180),
        action: Arc::new(move || {
            let context = Arc::clone(&context);
            Box::pin(async move {
                let enabled = two_bot_core::FeatureGates::from_env()
                    .map_err(|_| ErrorClass::Configuration)?
                    .announcements;
                let pool = context.pool().await?;
                MemberRuntime::run_birthday_tick(
                    pool,
                    &context.rest,
                    &context.guild,
                    two_bot_core::funnel::now_millis_for_test(),
                    enabled,
                )
                .await
            })
        }),
    })
}

/// Disabling new announcements must not disable compensation for roles that
/// already landed. The staging identity fence remains at registration.
pub(crate) async fn tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    now_ms: i64,
    enabled: bool,
) -> Result<(), ErrorClass> {
    if enabled {
        return MemberRuntime::run_birthday_sweep(pool, rest, guild, now_ms).await;
    }
    let _guard = store::lock(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    let mut failure = None;
    for grant in store::grants(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?
    {
        if let Err(error) = revoke(pool, rest, guild, &grant.member_id, &grant.role_id).await {
            failure = Some(error);
        }
    }
    if store::has_uncertain(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?
    {
        return Err(ErrorClass::RecoveryRequired);
    }
    failure.map_or(Ok(()), Err)
}

fn is_celebrant(profile: &store::Profile, date: Date) -> bool {
    let month = i16::from(u8::from(date.month()));
    let day = i16::from(date.day());
    profile.birth_month == month
        && (profile.birth_day == day
            || (month == 2
                && day == 28
                && profile.birth_day == 29
                && Date::from_calendar_date(date.year(), Month::February, 29).is_err()))
}

async fn roles(
    rest: &ActionExecutor,
    guild: &str,
    member: &str,
) -> Result<Option<HashSet<String>>, ErrorClass> {
    let Some(value) = rest
        .get_json_checked(&format!("/guilds/{guild}/members/{member}"))
        .await
        .map_err(|_| ErrorClass::Rest)?
    else {
        return Ok(None);
    };
    let roles = value["roles"].as_array().ok_or(ErrorClass::Rest)?;
    roles
        .iter()
        .map(|r| r.as_str().map(str::to_owned).ok_or(ErrorClass::Rest))
        .collect::<Result<HashSet<_>, _>>()
        .map(Some)
}

async fn revoke(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    member: &str,
    role: &str,
) -> Result<(), ErrorClass> {
    if roles(rest, guild, member)
        .await?
        .is_some_and(|r| r.contains(role))
    {
        rest.set_member_role(
            guild,
            member,
            role,
            false,
            "Birthday role expired or removed",
        )
        .await
        .map_err(|_| ErrorClass::Rest)?;
    }
    store::clear_grant(pool, guild, member, role)
        .await
        .map_err(|_| ErrorClass::Database)
}

pub(crate) async fn sweep(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    now_ms: i64,
) -> Result<(), ErrorClass> {
    let date = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now_ms) * 1_000_000)
        .map_err(|_| ErrorClass::Configuration)?
        .date();
    let _guard = store::lock(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    let settings = store::settings(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    let profiles = store::profiles(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    let celebrants: HashSet<&str> = profiles
        .iter()
        .filter(|p| is_celebrant(p, date))
        .map(|p| p.member_id.as_str())
        .collect();
    let mut failure = None;
    let role = match settings.role_id.as_deref() {
        Some(role) => match safe_role(rest, guild, role).await {
            Ok(true) => Some(role),
            Ok(false) => {
                failure = Some(ErrorClass::Configuration);
                None
            }
            Err(error) => {
                failure = Some(error);
                None
            }
        },
        None => None,
    };
    let grants = store::grants(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?;
    for grant in grants {
        if grant.celebration_date != date
            || role != Some(grant.role_id.as_str())
            || !celebrants.contains(grant.member_id.as_str())
        {
            if let Err(error) = revoke(pool, rest, guild, &grant.member_id, &grant.role_id).await {
                failure = Some(error)
            }
        }
    }
    let channel = match settings.channel_id.as_deref() {
        Some(channel) => match safe_channel(rest, guild, channel).await {
            Ok(true) => Some(channel),
            Ok(false) => {
                failure = Some(ErrorClass::Configuration);
                None
            }
            Err(error) => {
                failure = Some(error);
                None
            }
        },
        None => None,
    };
    // Profile ordering is numerical snowflake order (length, lexical), not text
    // order. The persistent count enforces 25/day even after edits or re-sweeps.
    for profile in &profiles {
        let member = profile.member_id.as_str();
        let today = celebrants.contains(member);
        let held = if role.is_some() || (today && channel.is_some()) {
            match roles(rest, guild, member).await {
                Ok(Some(held)) => held,
                Ok(None) => continue, // Member left; never announce them publicly.
                Err(error) => {
                    failure = Some(error);
                    continue;
                }
            }
        } else {
            HashSet::new()
        };
        if let Some(role) = role {
            let result = async {
                if today {
                    store::track_grant(pool, guild, member, role, date)
                        .await
                        .map_err(|_| ErrorClass::Database)?;
                    if !held.contains(role) {
                        rest.set_member_role(
                            guild,
                            member,
                            role,
                            true,
                            "Birthday cosmetic role for today",
                        )
                        .await
                        .map_err(|_| ErrorClass::Rest)?;
                    }
                } else if held.contains(role) {
                    // Also reconcile stale roles on enrolled non-celebrants,
                    // not just the grants this process originally recorded.
                    rest.set_member_role(
                        guild,
                        member,
                        role,
                        false,
                        "Stale birthday cosmetic role",
                    )
                    .await
                    .map_err(|_| ErrorClass::Rest)?;
                    store::clear_grant(pool, guild, member, role)
                        .await
                        .map_err(|_| ErrorClass::Database)?;
                }
                Ok::<(), ErrorClass>(())
            }
            .await;
            if let Err(error) = result {
                failure = Some(error)
            }
        }
        if !today {
            continue;
        }
        if let Some(channel) = channel {
            let hash = Sha256::digest(format!("birthday:{guild}:{member}:{date}"));
            let nonce = format!(
                "b{:024x}",
                u128::from_be_bytes(
                    hash[..16]
                        .try_into()
                        .map_err(|_| ErrorClass::Configuration)?
                ) & ((1u128 << 96) - 1)
            );
            if !store::claim_cheer(pool, guild, member, date, channel, &nonce)
                .await
                .map_err(|_| ErrorClass::Database)?
            {
                continue;
            }
            match rest
                .post_message_confirmed(channel, &render(settings.cheer.as_deref(), member), &nonce)
                .await
            {
                Ok(message) => {
                    store::posted(pool, guild, member, date, &message)
                        .await
                        .map_err(|_| ErrorClass::Database)?;
                    tracing::info!(guild_id = guild, member_id = member, "birthday.cheer");
                }
                Err(_) => {
                    // The fence stays, including on an unknown wire outcome.
                    // A recovery-required job result is visible in readiness.
                    tracing::warn!(
                        guild_id = guild,
                        member_id = member,
                        "birthday cheer requires reconciliation; send fence retained"
                    );
                    failure = Some(ErrorClass::RecoveryRequired);
                }
            }
        }
    }
    if store::has_uncertain(pool, guild)
        .await
        .map_err(|_| ErrorClass::Database)?
    {
        return Err(ErrorClass::RecoveryRequired);
    }
    failure.map_or(Ok(()), Err)
}
