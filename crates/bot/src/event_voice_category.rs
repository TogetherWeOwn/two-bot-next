//! Required event-voice category config + per-tick fail-closed preflight.
//!
//! Host decision D4: the event-voice category is REQUIRED configuration.
//! There is no default, nothing is auto-picked, and until a usable category
//! exists the feature is inactive: the website shows the TBD reason
//! `no_category`, no creates are attempted, and staff get one alert per
//! config gap (never per-tick spam). Every tick preflights Manage Channels and
//! View Channel on the configured category before any create; on 403 the tick
//! stays TBD with reason `perm_denied`, records the per-tick counter, alerts
//! once per episode, retries with backoff, and never crashes the tick.
//!
//! A moderator moving the live channel to another category is accepted: the
//! cached parent follows the channel (no fight), while future creates still
//! target the configured category. A move into a private category flips the
//! website link to members-only. Marking the event COMPLETED requires Manage
//! Events: on 403 the local handoff still completes and the PATCH is queued
//! for retry, so the after-party is never blocked on the event PATCH.
//!
//! The T-15 ticker (separate card) must only create on
//! [`PreflightDecision::Ready`]; website TBD rendering and the `web_v1`
//! columns belong to the web card, which consumes the reason vocabulary and
//! link flags pinned here.

use std::future::Future;
use std::time::{Duration, Instant};

use twilight_model::guild::Permissions;
use two_bot_core::metrics;
use two_bot_core::Snowflake;
use two_bot_discord::voice_rooms::RoomHttpError;

/// The one catalogued key this feature reads. Consulted exactly once per
/// load; there is no fallback key and no default category.
pub const EVENT_VOICE_CATEGORY_KEY: &str = "TWO_EVENT_VOICE_CATEGORY_ID";

/// How long a failed preflight waits before re-reading Discord. While the
/// backoff has not elapsed the gate returns the same [`PreflightDecision::Waiting`]
/// reason without calling the reader (the per-tick counter still records).
/// A steady-state permission denial (the category reads fine but the grant
/// lacks Manage Channels) is NOT backed off: it is rechecked every tick so
/// a moderator fixing the overwrite heals on the next tick.
pub const PREFLIGHT_DENIED_BACKOFF: Duration = Duration::from_secs(60);

/// How long a queued COMPLETED PATCH waits between retries.
pub const EVENT_PATCH_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Load the configured event-voice category id from exactly
/// [`EVENT_VOICE_CATEGORY_KEY`]. Missing, blank, unparsable, zero, and
/// non-canonical values (leading zeros, commas, negatives, trailing junk)
/// all fail closed to `None` — inactive, never a panic, never a guess.
pub fn configured_event_voice_category(env: impl Fn(&str) -> Option<String>) -> Option<Snowflake> {
    let raw = env(EVENT_VOICE_CATEGORY_KEY)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let id: Snowflake = trimmed.parse().ok()?;
    if id == 0 || id.to_string() != trimmed {
        return None;
    }
    Some(id)
}

/// Shared website TBD vocabulary for event voice. The web card renders
/// these; this card pins the strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventVoiceTbdReason {
    /// The event is too far out for a channel yet.
    OpensSoon,
    /// Discovery was rate-limited; transient, retried with backoff.
    RateLimited,
    /// The bot lacks Manage Channels + View Channel on the category.
    PermDenied,
    /// No usable category is configured (unset or deleted).
    NoCategory,
    /// Discovery failed transiently (transport/unknown outcome).
    Unavailable,
}

impl EventVoiceTbdReason {
    /// The exact website reason string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpensSoon => "opens_soon",
            Self::RateLimited => "rate_limited",
            Self::PermDenied => "perm_denied",
            Self::NoCategory => "no_category",
            Self::Unavailable => "unavailable",
        }
    }
}

/// What the gate needs to know about the configured category: its id, the
/// bot's effective permissions there (`None` when the snapshot is
/// incomplete — which fails closed), and whether `@everyone` can view it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategorySnapshot {
    pub id: Snowflake,
    pub bot_permissions: Option<Permissions>,
    pub everyone_can_view: bool,
}

/// Production: one category GET plus the bot's effective permissions there.
pub trait CategoryReader: Send + Sync {
    fn category_grant(
        &self,
        guild: Snowflake,
        category: Snowflake,
    ) -> impl Future<Output = Result<CategorySnapshot, RoomHttpError>> + Send;
}

/// One staff alert per episode. `category` is the configured id staff must
/// fix (`None` when nothing is configured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaffAlert {
    pub reason: EventVoiceTbdReason,
    pub category: Option<Snowflake>,
}

pub trait StaffAlertSink: Send + Sync {
    fn send_staff_alert(&self, alert: StaffAlert);
}

/// The gate's per-tick verdict. The ticker creates only on [`Self::Ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightDecision {
    /// The configured category is usable; create here.
    Ready { category: Snowflake },
    /// Stay TBD with this website reason; no create attempted.
    Waiting { reason: EventVoiceTbdReason },
}

/// Fail-closed preflight gate for the required event-voice category.
///
/// The gate owns the alert latch (one alert per episode for `no_category`
/// and `perm_denied` only; `rate_limited` and `unavailable` never alert),
/// the discovery backoff, and the mod-move cache. Failed preflights never
/// propagate an error: every failure mode maps to a [`PreflightDecision::Waiting`]
/// reason.
pub struct EventVoiceCategoryGate {
    configured: Option<Snowflake>,
    observed_parent: Option<Snowflake>,
    members_only: bool,
    alerted_no_category: bool,
    alerted_perm_denied: bool,
    backoff_until: Option<(Instant, EventVoiceTbdReason)>,
}

impl EventVoiceCategoryGate {
    /// A fresh gate for this configured id: no cached move, no backoff, no
    /// fired alerts.
    #[must_use]
    pub fn new(configured: Option<Snowflake>) -> Self {
        Self {
            configured,
            observed_parent: None,
            members_only: false,
            alerted_no_category: false,
            alerted_perm_denied: false,
            backoff_until: None,
        }
    }

    /// The configured category id, if any.
    #[must_use]
    pub fn configured(&self) -> Option<Snowflake> {
        self.configured
    }

    /// The live channel's cached parent: the configured id until a mod move
    /// is observed, the observed parent afterwards.
    #[must_use]
    pub fn effective_category(&self) -> Option<Snowflake> {
        self.observed_parent.or(self.configured)
    }

    /// Whether the website link must render members-only (the live channel
    /// currently sits in a private category).
    #[must_use]
    pub fn members_only_link(&self) -> bool {
        self.members_only
    }

    /// Where the next create goes: always the configured id. Mod moves never
    /// change the create target; `None` while unset.
    #[must_use]
    pub fn create_parent(&self) -> Option<Snowflake> {
        self.configured
    }

    /// Accept a moderator move: the cached parent follows the channel (no
    /// fight), and a move into a private category flips the website link to
    /// members-only until the channel is somewhere public again.
    pub fn note_mod_move(&mut self, observed_parent: Snowflake, everyone_can_view: bool) {
        self.observed_parent = Some(observed_parent);
        self.members_only = !everyone_can_view;
    }

    /// Apply a (per-tick) env re-read. A changed id is a fresh episode:
    /// backoff, the cached move, and the alert latch reset. The same id
    /// keeps them, so the re-read never re-spams.
    pub fn reconfigure(&mut self, configured: Option<Snowflake>) {
        if configured == self.configured {
            return;
        }
        self.configured = configured;
        self.observed_parent = None;
        self.members_only = false;
        self.alerted_no_category = false;
        self.alerted_perm_denied = false;
        self.backoff_until = None;
    }

    fn record_tick(reason: EventVoiceTbdReason) {
        metrics::global().event_voice_tick(reason.as_str());
    }

    fn maybe_alert(&mut self, alerts: &impl StaffAlertSink, reason: EventVoiceTbdReason) {
        let latched = match reason {
            EventVoiceTbdReason::NoCategory => &mut self.alerted_no_category,
            EventVoiceTbdReason::PermDenied => &mut self.alerted_perm_denied,
            // Transient TBDs never page staff.
            EventVoiceTbdReason::RateLimited
            | EventVoiceTbdReason::Unavailable
            | EventVoiceTbdReason::OpensSoon => return,
        };
        if *latched {
            return;
        }
        *latched = true;
        alerts.send_staff_alert(StaffAlert {
            reason,
            category: self.configured,
        });
    }

    fn backoff_reason(&self, now: Instant) -> Option<EventVoiceTbdReason> {
        let (failed_at, reason) = self.backoff_until?;
        let elapsed = now
            .checked_duration_since(failed_at)
            .unwrap_or(Duration::ZERO);
        (elapsed < PREFLIGHT_DENIED_BACKOFF).then_some(reason)
    }

    /// Preflight the configured category for this tick. `None` configured
    /// returns `Waiting(NoCategory)` without a reader call; discovery
    /// failures map to their TBD reason with backoff; a snapshot whose
    /// permissions lack `VIEW_CHANNEL | MANAGE_CHANNELS` (or are `None`)
    /// returns `Waiting(PermDenied)` and is rechecked every tick. Every
    /// `Waiting` records one per-tick counter observation, including
    /// backoff-skipped ticks. A `Ready` re-arms the alert latch. Never
    /// propagates an error and never panics.
    pub async fn preflight(
        &mut self,
        reader: &impl CategoryReader,
        alerts: &impl StaffAlertSink,
        guild: Snowflake,
        now: Instant,
    ) -> PreflightDecision {
        let Some(category) = self.configured else {
            Self::record_tick(EventVoiceTbdReason::NoCategory);
            self.maybe_alert(alerts, EventVoiceTbdReason::NoCategory);
            return PreflightDecision::Waiting {
                reason: EventVoiceTbdReason::NoCategory,
            };
        };
        if let Some(reason) = self.backoff_reason(now) {
            Self::record_tick(reason);
            return PreflightDecision::Waiting { reason };
        }
        match reader.category_grant(guild, category).await {
            Ok(snapshot) => {
                let granted = snapshot
                    .bot_permissions
                    .is_some_and(|perms| perms.contains(required_category_grant()));
                if granted {
                    self.backoff_until = None;
                    self.alerted_no_category = false;
                    self.alerted_perm_denied = false;
                    return PreflightDecision::Ready { category };
                }
                // Cheap steady-state denial: no backoff, so a fixed
                // overwrite heals on the next tick.
                Self::record_tick(EventVoiceTbdReason::PermDenied);
                self.maybe_alert(alerts, EventVoiceTbdReason::PermDenied);
                PreflightDecision::Waiting {
                    reason: EventVoiceTbdReason::PermDenied,
                }
            }
            Err(error) => {
                let reason = match error {
                    RoomHttpError::NotFound => EventVoiceTbdReason::NoCategory,
                    RoomHttpError::AccessDenied => EventVoiceTbdReason::PermDenied,
                    RoomHttpError::RateLimited { .. } => EventVoiceTbdReason::RateLimited,
                    // Any other transport failure (including Unauthorized:
                    // the tick must survive even a refused credential).
                    _ => EventVoiceTbdReason::Unavailable,
                };
                self.backoff_until = Some((now, reason));
                Self::record_tick(reason);
                self.maybe_alert(alerts, reason);
                PreflightDecision::Waiting { reason }
            }
        }
    }
}

/// The category grant a tick needs before any create: see the category and
/// manage channels in it.
fn required_category_grant() -> Permissions {
    Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS
}

/// Production: PATCH the scheduled event to COMPLETED.
pub trait EventStatusPatcher: Send + Sync {
    fn patch_event_completed(
        &self,
        guild: Snowflake,
        event: Snowflake,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
}

/// The PATCH helper's verdict. It returns normally in every case — never
/// failing the tick — so the caller completes the local handoff first and
/// the after-party is never blocked on the event PATCH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    /// The event is COMPLETED (or gone, which is moot): no further calls.
    Published,
    /// Still unpublished; retry after [`EVENT_PATCH_RETRY_AFTER`].
    QueuedForRetry,
}

/// A COMPLETED PATCH that must eventually land. `AccessDenied` and
/// `RateLimited` (and any other transport failure) queue for retry with
/// backoff; `NotFound` (event gone) resolves as [`PatchOutcome::Published`]
/// instead of 404ing forever; success publishes. A published patch makes no
/// further wire calls.
pub struct PendingCompletedPatch {
    guild: Snowflake,
    event: Snowflake,
    published: bool,
    queued_at: Option<Instant>,
}

impl PendingCompletedPatch {
    /// A fresh patch for this event: unpublished, nothing queued yet.
    #[must_use]
    pub fn for_event(guild: Snowflake, event: Snowflake) -> Self {
        Self {
            guild,
            event,
            published: false,
            queued_at: None,
        }
    }

    /// The event this patch targets.
    #[must_use]
    pub fn event(&self) -> Snowflake {
        self.event
    }

    /// Whether a failed attempt is queued for retry.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.published && self.queued_at.is_some()
    }

    /// Drive the PATCH toward published. Skips the wire call while a queued
    /// retry's backoff has not elapsed, and after publishing.
    pub async fn ensure_published(
        &mut self,
        patcher: &impl EventStatusPatcher,
        now: Instant,
    ) -> PatchOutcome {
        if self.published {
            return PatchOutcome::Published;
        }
        if let Some(queued_at) = self.queued_at {
            let elapsed = now
                .checked_duration_since(queued_at)
                .unwrap_or(Duration::ZERO);
            if elapsed < EVENT_PATCH_RETRY_AFTER {
                return PatchOutcome::QueuedForRetry;
            }
        }
        match patcher.patch_event_completed(self.guild, self.event).await {
            Ok(()) | Err(RoomHttpError::NotFound) => {
                self.published = true;
                self.queued_at = None;
                PatchOutcome::Published
            }
            Err(_) => {
                self.queued_at = Some(now);
                PatchOutcome::QueuedForRetry
            }
        }
    }
}

#[cfg(test)]
#[path = "event_voice_category_regression_tests.rs"]
mod regression_tests;
