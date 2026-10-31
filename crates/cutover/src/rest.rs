//! Discord REST seam for the cutover tools.
//!
//! Ports `src/discord/rest.ts` onto twilight-http. Semantics preserved: a
//! 110ms pacing floor between requests (Discord allows 50/s globally; the
//! tools use far less), 404 is unreadable (not an abort), 429 backs
//! off, 5xx retries with exponential backoff (four retries). History scans
//! retain partial pages and an explicit completion reason. Single-model
//! reads refuse lost access (403) and spent retry budgets as errors —
//! only the archived-thread walk translates a refusal into its legacy
//! walk-end, and only genuine absence reads as `None`.
//! `requests` counts calls so each run reports its
//! own Discord cost.
//!
//! `proxy_url` overrides the API host (twilight's `ClientBuilder::proxy`,
//! the analogue of legacy `DISCORD_API_BASE`): verification runs point the
//! tools at a local mock instead of Discord.

use sqlx::postgres::PgConnectOptions;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use twilight_http::error::Error as TwilightError;
use twilight_http::request::{AuditLogReason, Request, TryIntoRequest};
use twilight_http::Client;
use twilight_model::channel::message::Message;
use twilight_model::channel::Channel;
use twilight_model::guild::invite::Invite;
use twilight_model::guild::{Guild, Member};
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker};
use twilight_model::id::Id;
use two_bot_core::database_tls::{self, TlsPolicy};
use two_bot_core::send_admission::{PgSendAdmission, SendAdmission};
use two_bot_discord::executor::HyperTransport;

/// REST failure: transport/validation vs Discord API error vs body decode.
#[derive(Debug, Error)]
pub enum RestError {
    #[error("discord request failed: {0}")]
    Twilight(#[from] TwilightError),
    #[error("discord response body unreadable: {0}")]
    Body(#[from] serde_json::Error),
    #[error("discord send refused: {0}")]
    Wire(String),
    /// Discord refused the read (lost access): the resource may exist but
    /// this token cannot see it. Strict readers fail; the archived-thread
    /// walk ends where it stands.
    #[error("discord read refused: HTTP {status}")]
    Refused { status: u16 },
    /// A full member page did not advance the `after` cursor (repeated or
    /// out-of-order page): traversal cannot be proven complete.
    #[error("member pagination stalled: page did not advance the cursor")]
    MemberCursorStalled,
    /// A page/member ceiling was hit while Discord still reported full pages.
    #[error("member pagination exceeded ceiling ({pages} pages, {members} members)")]
    MemberCeilingExceeded { pages: u32, members: usize },
}

/// Default ceiling on member pages per traversal (1000 members/page, so
/// 500k members: far beyond any TWO guild yet a hard memory/request bound).
pub const MAX_MEMBER_PAGES: u32 = 500;
/// Default ceiling on accumulated members per traversal.
pub const MAX_MEMBERS: usize = 500_000;

/// What a channel delete proved about the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// 2xx: this call deleted the channel.
    Deleted,
    /// 404: the channel was already gone; nothing was deleted.
    AlreadyGone,
}

/// Send-admission pool shape for the operator tools: max 2 + 15 s statement
/// timeout with a 10 s acquire timeout (website-jobs admission parity,
/// threat-model F6).
const ADMISSION_POOL_MAX: u32 = 2;
const ADMISSION_STATEMENT_TIMEOUT_MS: u64 = 15_000;
const ADMISSION_ACQUIRE_TIMEOUT_SECS: u64 = 10;

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<TlsPolicy, RestError> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
        .map_err(|_| RestError::Wire("admission authority unavailable".to_owned()))
}

/// Shared fenced options builder: validate before SQLx can WARN-log query
/// values, enforce the TLS policy, then apply the effective mode. The
/// statement timeout rides the connection options. Failures are fixed
/// strings; nothing echoes the URL, host or credential.
fn admission_connect_options(url: &str, tls: TlsPolicy) -> Result<PgConnectOptions, &'static str> {
    two_bot_core::database_url::validate(url)?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    database_tls::enforce(url, tls)?;
    let options = two_bot_core::database_url::connect_options(url)
        .map_err(|_| "admission authority unavailable")?;
    let options = database_tls::apply(options, tls);
    Ok(options.options([(
        "statement_timeout",
        format!("{ADMISSION_STATEMENT_TIMEOUT_MS}ms"),
    )]))
}

/// Paced twilight client with legacy retry semantics.
#[derive(Debug, Clone)]
pub struct RestClient {
    inner: Arc<RestInner>,
}

#[derive(Debug)]
struct RestInner {
    client: Client,
    transport: Result<HyperTransport, String>,
    min_interval: Duration,
    last_at: tokio::sync::Mutex<std::time::Instant>,
    requests: AtomicU64,
}

impl RestClient {
    /// Ungoverned compatibility constructor; live sends are refused. Runtime
    /// CLI callers use `from_env` to bind the durable authority.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self::with_proxy(token, None)
    }

    /// Build with an optional API-host override (mock verification).
    /// Accepts a full origin (`http://127.0.0.1:PORT`) or bare host:port:
    /// twilight's proxy takes the HOST only (`http://{host}/api/v10/...`),
    /// so any scheme prefix is stripped here.
    #[must_use]
    pub fn with_proxy(token: String, proxy_url: Option<String>) -> Self {
        Self::build(token, proxy_url, None)
    }

    pub fn with_admission(
        token: String,
        proxy_url: Option<String>,
        admission: Arc<dyn SendAdmission>,
    ) -> Self {
        Self::build(token, proxy_url, Some(admission))
    }

    /// Administrative data targets may differ; admission must use the runtime's
    /// TWO_DATABASE_URL authority, never the backfill/capture target database.
    /// The pool is fenced by the `TWO_DATABASE_TLS` policy (unset means
    /// `required`): refusals read as the same redacted authority error, with
    /// no oracle for which fence tripped and no URL part in the message.
    pub async fn from_env(token: String, proxy_url: Option<String>) -> Result<Self, RestError> {
        let url = std::env::var("TWO_DATABASE_URL").map_err(|_| {
            RestError::Wire("TWO_DATABASE_URL admission authority required".to_owned())
        })?;
        let tls = tls_policy_from_env()?;
        let options = admission_connect_options(&url, tls)
            .map_err(|_| RestError::Wire("admission authority unavailable".to_owned()))?;
        Self::from_options(token, proxy_url, options).await
    }

    /// [`Self::from_env`] with an explicit database URL and TLS policy (tests
    /// pass `LocalOnly`). Refusals are fixed strings with no URL, host or
    /// credential part, raised before SQLx parses the URL or opens a socket.
    /// Eager: the pool connects before the gate binds, so a dead authority
    /// fails here rather than on the first send.
    pub async fn from_url_with_tls(
        token: String,
        proxy_url: Option<String>,
        url: &str,
        tls: TlsPolicy,
    ) -> Result<Self, RestError> {
        let options =
            admission_connect_options(url, tls).map_err(|e| RestError::Wire(e.to_owned()))?;
        Self::from_options(token, proxy_url, options).await
    }

    /// Eager pool connect plus gate bind for the fenced options above.
    async fn from_options(
        token: String,
        proxy_url: Option<String>,
        options: PgConnectOptions,
    ) -> Result<Self, RestError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(ADMISSION_POOL_MAX)
            .acquire_timeout(Duration::from_secs(ADMISSION_ACQUIRE_TIMEOUT_SECS))
            .connect_with(options)
            .await
            .map_err(|_| RestError::Wire("admission authority unavailable".to_owned()))?;
        let gate =
            PgSendAdmission::new(pool, &token).map_err(|e| RestError::Wire(e.to_string()))?;
        Ok(Self::with_admission(token, proxy_url, Arc::new(gate)))
    }

    fn build(
        token: String,
        proxy_url: Option<String>,
        admission: Option<Arc<dyn SendAdmission>>,
    ) -> Self {
        let transport = match admission {
            Some(gate) => HyperTransport::with_admission(token.clone(), proxy_url.clone(), gate),
            None => HyperTransport::with_proxy(token.clone(), proxy_url.clone()),
        };
        let mut builder = Client::builder().token(token).ratelimiter(None);
        if let Some(url) = proxy_url {
            let host = url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/');
            builder = builder.proxy(host.to_owned(), true);
        }
        Self {
            inner: Arc::new(RestInner {
                client: builder.build(),
                transport,
                min_interval: Duration::from_millis(110),
                last_at: tokio::sync::Mutex::new(
                    std::time::Instant::now() - Duration::from_secs(60),
                ),
                requests: AtomicU64::new(0),
            }),
        }
    }

    /// Requests made so far: the run's own Discord cost report.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    async fn pace(&self) {
        let mut last = self.inner.last_at.lock().await;
        let earliest = *last + self.inner.min_interval;
        let now = std::time::Instant::now();
        if earliest > now {
            tokio::time::sleep(earliest - now).await;
        }
        *last = std::time::Instant::now();
    }

    /// Twilight is only a request factory: ResponseFuture hides 429 resends.
    /// Every actual attempt instead passes through the governed raw transport,
    /// preserving the legacy classification — 403/404 give up as unreadable,
    /// 429 parks per the response's own retry-after, 5xx backs off and then
    /// gives up as retry-exhausted, anything else fails.
    async fn exec_list<T, F>(&self, make: F) -> Result<ListPage<T>, RestError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> Result<Request, TwilightError>,
    {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        let mut attempt: u32 = 0;
        loop {
            self.pace().await;
            let request = make()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            // The 30 s page budget is the transport's absolute deadline —
            // durable admission, headers and body share it — so a stalled GET
            // times out where its permit is in scope and releases the lane.
            // No outer timeout may wrap this call (it would orphan the lane).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let (res, _) = transport
                .send_request(&request, Some(deadline))
                .await
                .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(ListPage::Read(serde_json::from_slice(&res.body)?)),
                403 | 404 => return Ok(ListPage::Interrupted(ScanCompletion::Unreadable)),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => return Ok(ListPage::Interrupted(ScanCompletion::RetryExhausted)),
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
            attempt += 1;
        }
    }

    /// Run one single-model request with the same transport-governed retry
    /// semantics. Lost access (403) refuses and a spent 5xx budget errors —
    /// never a silent `None`; only genuine absence (404) reads as `None`.
    async fn exec_one<T, F>(&self, make: F) -> Result<Option<T>, RestError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> Result<Request, TwilightError>,
    {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        for attempt in 0..=4 {
            self.pace().await;
            let request = make()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            // Same absolute-deadline shape as the page scan above.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let (res, _) = transport
                .send_request(&request, Some(deadline))
                .await
                .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(Some(serde_json::from_slice(&res.body)?)),
                403 => return Err(RestError::Refused { status: 403 }),
                404 => return Ok(None),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => {
                    return Err(RestError::Wire("HTTP 5xx retry exhausted".to_owned()));
                }
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
        }
        unreachable!("last attempt always returns")
    }

    /// Page a guild's full member list (`joined_at` is Discord's own record).
    /// Returns `None` when a page is unreadable (strict callers treat that
    /// as failure, not as end-of-list — legacy `fetchAllMembersStrict`).
    /// Traversal is bounded by [`MAX_MEMBER_PAGES`] / [`MAX_MEMBERS`]; a
    /// non-advancing cursor or a ceiling hit is an `Err`, never a partial
    /// roster presented as complete.
    pub async fn fetch_all_members(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Member>>, RestError> {
        self.fetch_all_members_bounded(guild_id, MAX_MEMBER_PAGES, MAX_MEMBERS)
            .await
    }

    /// [`Self::fetch_all_members`] with explicit ceilings (tests, tuning).
    pub async fn fetch_all_members_bounded(
        &self,
        guild_id: Id<GuildMarker>,
        max_pages: u32,
        max_members: usize,
    ) -> Result<Option<Vec<Member>>, RestError> {
        let mut out = Vec::new();
        let mut after: Option<Id<UserMarker>> = None;
        let mut pages: u32 = 0;
        loop {
            if pages >= max_pages || out.len() >= max_members {
                return Err(RestError::MemberCeilingExceeded {
                    pages,
                    members: out.len(),
                });
            }
            let page: ListPage<Member> = self
                .exec_list(|| {
                    let client = &self.inner.client;
                    let mut req = client.guild_members(guild_id).limit(1000);
                    if let Some(a) = after {
                        req = req.after(a);
                    }
                    req.try_into_request()
                })
                .await?;
            let ListPage::Read(batch) = page else {
                return Ok(None);
            };
            pages += 1;
            if batch.is_empty() {
                break;
            }
            let full = batch.len() >= 1000;
            let last = batch.last().map(|m| m.user.id);
            out.extend(batch);
            match (last, full) {
                (Some(id), true) => {
                    if after.is_some_and(|prev| id <= prev) {
                        return Err(RestError::MemberCursorStalled);
                    }
                    after = Some(id);
                }
                _ => break,
            }
        }
        Ok(Some(out))
    }

    /// One member-list page as raw JSON for the member-catch-up slice
    /// (TOG-22941): `limit` is the caller's 1-1000 page size, `after` is the
    /// resume cursor carried verbatim onto the wire. Rides the same governed
    /// transport as every page (pacing, retries, `requests()` counting), so
    /// the catch-up proof that every request passed through the rate-limit
    /// client holds. An unreadable page (403/404) or a 5xx that is still
    /// failing after the last retry is an error, never an empty page: only
    /// a real `200 []` ends a walk, so a Discord outage mid-catch-up cannot
    /// read as a finished walk. Strict traversal callers keep using
    /// [`Self::fetch_all_members`].
    pub async fn fetch_members_page_json(
        &self,
        guild_id: Id<GuildMarker>,
        limit: u16,
        after: Option<Id<UserMarker>>,
    ) -> Result<Vec<serde_json::Value>, RestError> {
        let page: ListPage<serde_json::Value> = self
            .exec_list(|| {
                let client = &self.inner.client;
                let mut req = client.guild_members(guild_id).limit(limit);
                if let Some(cursor) = after {
                    req = req.after(cursor);
                }
                req.try_into_request()
            })
            .await?;
        match page {
            ListPage::Read(batch) => Ok(batch),
            ListPage::Interrupted(reason) => Err(RestError::Wire(format!(
                "member list page unreadable: {reason}"
            ))),
        }
    }

    /// Guild invites (attribution baseline / capture window counters).
    pub async fn guild_invites(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Invite>>, RestError> {
        self.exec_list(|| {
            let client = &self.inner.client;
            client.guild_invites(guild_id).try_into_request()
        })
        .await
        .map(ListPage::into_option)
    }

    /// Guild channels (backfill candidate scan / message-ladder targets).
    pub async fn guild_channels(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Channel>>, RestError> {
        self.exec_list(|| {
            let client = &self.inner.client;
            client.guild_channels(guild_id).try_into_request()
        })
        .await
        .map(ListPage::into_option)
    }

    /// Delete one channel (ghost-cleanup execute path), carrying the
    /// operator's `reason` in the Discord audit-log header. Same governed
    /// retry semantics as the readers, minus body decode (DELETE answers
    /// 204 with no body): 2xx is deleted, 404 is already-gone success (a
    /// duplicate delete is success), 429 backs off on the response's own
    /// retry-after, 5xx retries with backoff. A 403 is a hard failure —
    /// never proof of deletion, so the row stays tracked for the next run.
    pub async fn delete_channel(
        &self,
        channel_id: Id<ChannelMarker>,
        reason: &str,
    ) -> Result<DeleteOutcome, RestError> {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        for attempt in 0..=4 {
            self.pace().await;
            let request = self
                .inner
                .client
                .delete_channel(channel_id)
                .reason(reason)
                .try_into_request()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            // Absolute deadline covers admission, headers and body. A DELETE
            // timeout keeps the permit held (uncertain mutation) by design.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let (res, _) = transport
                .send_request(&request, Some(deadline))
                .await
                .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(DeleteOutcome::Deleted),
                404 => return Ok(DeleteOutcome::AlreadyGone),
                403 => return Err(RestError::Wire("HTTP 403".to_owned())),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => return Err(RestError::Wire("HTTP 5xx retry exhausted".to_owned())),
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
        }
        unreachable!("last attempt always returns")
    }

    /// Guild fetch (vanity-URL presence for attribution). Lost access and a
    /// spent retry budget are errors; only genuine absence is `None`.
    pub async fn guild(&self, guild_id: Id<GuildMarker>) -> Result<Option<Guild>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            client.guild(guild_id).try_into_request()
        })
        .await
    }

    /// Active threads in a guild (forum posts live in threads). Lost access
    /// and a spent retry budget are errors; only genuine absence is `None`.
    pub async fn active_threads(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<twilight_model::channel::thread::ThreadsListing>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            client.active_threads(guild_id).try_into_request()
        })
        .await
    }

    /// Public archived threads of a channel (forum history).
    ///
    /// Follows the listing's own continuation the way Discord documents it:
    /// each page carries `has_more`, and the next page passes `before` with
    /// the oldest thread's `archive_timestamp` (listings run newest-first).
    /// Returns every thread exactly once by snowflake; a refused first page
    /// reports `Ok(None)` (legacy: an unreadable channel is a normal
    /// condition), while a refused later page, a repeated or
    /// non-progressing continuation and a spent page budget report
    /// [`ArchivedThreadsOutcome::Incomplete`] with the threads seen so
    /// far — never an infinite loop or a silently complete result. A
    /// spent 5xx budget propagates as an error.
    pub async fn public_archived_threads(
        &self,
        channel_id: Id<ChannelMarker>,
    ) -> Result<Option<twilight_model::channel::thread::ThreadsListing>, RestError> {
        Ok(self
            .list_all_archived_threads(channel_id, DEFAULT_ARCHIVED_THREAD_PAGES)
            .await?
            .map(|outcome| outcome.into_listing()))
    }

    /// Bounded archived-thread walk with explicit completion evidence.
    /// `max_pages` is the same hard cost ceiling [`Self::scan_channel`] uses:
    /// hitting it reports `incomplete` rather than dropping older threads
    /// silently. Reports `Ok(None)` when the first page is refused, so
    /// the compat helper keeps the legacy no-listing contract; a later
    /// refused page ends the walk [`ArchivedThreadsOutcome::Incomplete`]
    /// with [`ArchiveIncompleteReason::AccessLost`] and the threads seen
    /// so far, so the caller records the truncation.
    pub async fn list_all_archived_threads(
        &self,
        channel_id: Id<ChannelMarker>,
        max_pages: usize,
    ) -> Result<Option<ArchivedThreadsOutcome>, RestError> {
        let mut threads: Vec<Channel> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        let mut before: Option<String> = None;
        let mut pages = 0usize;
        let mut last_cursor: Option<String> = None;
        let mut refused = false;

        loop {
            if pages >= max_pages {
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::PageBudget,
                }));
            }
            let page: Option<twilight_model::channel::thread::ThreadsListing> = match self
                .exec_one(|| {
                    let client = &self.inner.client;
                    match before.as_deref() {
                        Some(cursor) => client
                            .public_archived_threads(channel_id)
                            .limit(100)
                            .before(cursor)
                            .try_into_request(),
                        None => client
                            .public_archived_threads(channel_id)
                            .limit(100)
                            .try_into_request(),
                    }
                })
                .await
            {
                Ok(page) => page,
                // Lost access ends the walk where it stands: no listing when
                // the first page refuses (legacy walk-end), an incomplete
                // walk with a refusal reason afterwards. A spent retry
                // budget is transient sickness, not stable unreadability,
                // so it propagates as an error.
                Err(RestError::Refused { .. }) => {
                    refused = true;
                    None
                }
                Err(error) => return Err(error),
            };
            let Some(listing) = page else {
                // Nothing was readable at all: stay `None`, exactly like the
                // legacy single-page consumer. A later refused page ends the
                // walk incomplete — `has_more` never cleared, so reporting
                // `Complete` would silently drop the unseen remainder. A
                // later absent page (404: the channel is gone, not hidden)
                // keeps the legacy complete-with-seen walk-end.
                if threads.is_empty() {
                    return Ok(None);
                }
                if refused {
                    return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                        threads,
                        reason: ArchiveIncompleteReason::AccessLost,
                    }));
                }
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            };
            if listing.threads.is_empty() {
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            }
            pages += 1;
            for thread in &listing.threads {
                if seen.insert(thread.id.get()) {
                    threads.push(thread.clone());
                }
            }
            let has_more = listing.has_more.unwrap_or(false);
            if !has_more {
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            }
            let Some(cursor) = oldest_archive_timestamp(&listing) else {
                // `has_more` with no usable cursor cannot advance: report the
                // partial walk instead of re-requesting the same page forever.
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::NoProgress,
                }));
            };
            if last_cursor.as_deref() == Some(cursor.as_str()) {
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::NoProgress,
                }));
            }
            last_cursor = Some(cursor.clone());
            before = Some(cursor);
        }
    }

    /// Walk a channel's history newest-first (legacy `scanChannel`).
    /// `stop_before`: stop once older than this timestamp (millis). The
    /// caller converts ISO bounds to millis via [`iso_to_millis`].
    /// `max_pages` is a hard cost ceiling — hitting it reports `truncated`.
    pub async fn scan_channel(
        &self,
        channel_id: Id<ChannelMarker>,
        max_pages: usize,
        stop_before_ms: Option<i64>,
    ) -> Result<ScanPage, RestError> {
        let mut messages: Vec<Message> = Vec::new();
        let mut before: Option<Id<MessageMarker>> = None;
        let mut pages = 0usize;
        let completion = loop {
            if pages >= max_pages {
                break ScanCompletion::PageCap;
            }
            let page = self
                .exec_list(|| {
                    let client = &self.inner.client;
                    match before {
                        Some(b) => client
                            .channel_messages(channel_id)
                            .limit(100)
                            .before(b)
                            .try_into_request(),
                        None => client
                            .channel_messages(channel_id)
                            .limit(100)
                            .try_into_request(),
                    }
                })
                .await;
            let batch: Vec<Message> = match page {
                Ok(ListPage::Read(batch)) => batch,
                Ok(ListPage::Interrupted(reason)) => break reason,
                Err(RestError::Body(_)) => break ScanCompletion::InvalidResponse,
                Err(RestError::Twilight(_)) => break ScanCompletion::RequestFailed,
                Err(RestError::Wire(_)) => break ScanCompletion::RequestFailed,
                // Member-pagination errors and single-model refusals never come
                // from `exec_list`; propagate rather than mislabel a scan
                // completion. A refusal is lost access: failing the scan is
                // fail-closed, never end-of-history.
                Err(
                    e @ (RestError::MemberCursorStalled
                    | RestError::MemberCeilingExceeded { .. }
                    | RestError::Refused { .. }),
                ) => return Err(e),
            };
            if batch.is_empty() {
                break ScanCompletion::EndOfHistory;
            }
            pages += 1;
            let short = batch.len() < 100;
            before = batch.last().map(|m| m.id);
            let past_stop = stop_before_ms.is_some_and(|bound| {
                batch
                    .last()
                    .is_some_and(|m| timestamp_ms(&m.timestamp) < bound)
            });
            messages.extend(batch);
            // A short page proves history ended, even if it also crosses the bound.
            if short {
                break ScanCompletion::EndOfHistory;
            }
            if past_stop {
                break ScanCompletion::TimeBoundary;
            }
        };

        let scanned_back_to = messages.last().map(|m| m.timestamp.iso_8601().to_string());
        Ok(ScanPage {
            messages,
            scanned_back_to,
            truncated: completion == ScanCompletion::PageCap,
            completion,
        })
    }
}

/// A failed list page is not an empty list. Non-history callers retain their
/// legacy optional result at the boundary, not inside the retry executor.
enum ListPage<T> {
    Read(Vec<T>),
    Interrupted(ScanCompletion),
}

impl<T> ListPage<T> {
    fn into_option(self) -> Option<Vec<T>> {
        match self {
            Self::Read(batch) => Some(batch),
            Self::Interrupted(_) => None,
        }
    }
}

/// Deterministic 5xx backoff base in millis: `500 * 2^attempt`, saturating
/// the shift at 10 (512 s cap).
fn backoff_base_ms(attempt: u32) -> u64 {
    500u64.saturating_mul(1u64 << attempt.min(10))
}

fn backoff_duration(attempt: u32) -> Duration {
    Duration::from_millis(backoff_base_ms(attempt))
}

/// Deterministic 64-bit mixer (splitmix64) for seeded jitter without a
/// global RNG: the same `(seed, attempt)` pair always yields the same draw,
/// on every platform, with no clock and no `rand` dependency.
fn jitter_draw(seed: u64, attempt: u32) -> u64 {
    let mixed = seed.wrapping_add(u64::from(attempt).wrapping_mul(0x9E3779B97F4A7C15));
    let mut z = mixed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// 5xx backoff for `attempt` (0-based) with full jitter: a deterministic
/// draw in `[1 ms, backoff_base_ms(attempt)]`, so concurrent scans retrying
/// the same outage spread instead of re-hammering in lockstep. The
/// deterministic base stays as the bound (the 512 s saturation cap and the
/// four-retry budgets hold by construction); the same `seed` replays the
/// same sample, so tests pin ranges without a clock or RNG.
#[must_use]
pub fn backoff_duration_jittered(attempt: u32, seed: u64) -> Duration {
    let base_ms = backoff_base_ms(attempt);
    Duration::from_millis(1 + (jitter_draw(seed, attempt) % base_ms))
}

/// ISO-8601 → epoch millis for scan bounds.
#[must_use]
pub fn iso_to_millis(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// twilight `Timestamp` → epoch millis.
#[must_use]
pub fn timestamp_ms(ts: &twilight_model::util::datetime::Timestamp) -> i64 {
    ts.as_micros() / 1000
}

/// Why a history walk stopped. A time boundary completes the requested window,
/// not necessarily the entire history; a page cap or interruption is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanCompletion {
    EndOfHistory,
    TimeBoundary,
    PageCap,
    Unreadable,
    RetryExhausted,
    RequestFailed,
    InvalidResponse,
}

impl ScanCompletion {
    #[must_use]
    pub fn interrupted(self) -> bool {
        matches!(
            self,
            Self::Unreadable | Self::RetryExhausted | Self::RequestFailed | Self::InvalidResponse
        )
    }
}

impl std::fmt::Display for ScanCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EndOfHistory => "end-of-history",
            Self::TimeBoundary => "time-boundary",
            Self::PageCap => "page-cap",
            Self::Unreadable => "unreadable",
            Self::RetryExhausted => "retry-exhausted",
            Self::RequestFailed => "request-failed",
            Self::InvalidResponse => "invalid-response",
        })
    }
}

/// One channel walk outcome (legacy `ScanResult` plus explicit completion).
#[derive(Debug, Clone)]
pub struct ScanPage {
    pub messages: Vec<Message>,
    pub scanned_back_to: Option<String>,
    /// Legacy page-cap flag only; interruptions are described by `completion`.
    pub truncated: bool,
    pub completion: ScanCompletion,
}

/// Default page budget for archived-thread discovery: a 130-thread forum
/// needs two limit(100) pages, so two pages complete the common case while
/// a stuck continuation still terminates quickly.
pub const DEFAULT_ARCHIVED_THREAD_PAGES: usize = 10;

/// Why archived-thread discovery stopped with threads potentially unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveIncompleteReason {
    /// The page budget ran out before `has_more` cleared.
    PageBudget,
    /// The next cursor repeated or the listing offered no cursor to follow.
    NoProgress,
    /// Discord refused a later page (403): access was lost after threads
    /// were already collected, so the unseen remainder is unknown.
    AccessLost,
}

/// Bounded archived-thread discovery outcome: either every page through
/// `has_more == false`, or the partial walk plus why it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedThreadsOutcome {
    Complete {
        threads: Vec<Channel>,
    },
    Incomplete {
        threads: Vec<Channel>,
        reason: ArchiveIncompleteReason,
    },
}

impl ArchivedThreadsOutcome {
    /// Threads discovered so far, complete or not.
    #[must_use]
    pub fn threads(&self) -> &[Channel] {
        match self {
            Self::Complete { threads } | Self::Incomplete { threads, .. } => threads,
        }
    }

    /// True only when `has_more` cleared and every continuation progressed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete { .. })
    }

    /// Back-compat single listing for the legacy one-page call site: the
    /// threads seen so far. `list_all_archived_threads` returns `None`
    /// instead of calling this when nothing was readable at all.
    #[must_use]
    pub fn into_listing(self) -> twilight_model::channel::thread::ThreadsListing {
        let (threads, complete) = match self {
            Self::Complete { threads } => (threads, true),
            Self::Incomplete { threads, .. } => (threads, false),
        };
        twilight_model::channel::thread::ThreadsListing {
            has_more: Some(!complete),
            members: Vec::new(),
            threads,
        }
    }
}

/// Oldest `archive_timestamp` in a listing: the `before` cursor for the next
/// page. Listings run newest-first, so the last thread carries the cursor.
fn oldest_archive_timestamp(
    listing: &twilight_model::channel::thread::ThreadsListing,
) -> Option<String> {
    listing
        .threads
        .iter()
        .filter_map(|t| t.thread_metadata.as_ref())
        .map(|m| m.archive_timestamp.iso_8601().to_string())
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use twilight_model::channel::thread::{AutoArchiveDuration, ThreadMetadata, ThreadsListing};
    use twilight_model::channel::{Channel, ChannelType};

    const FORUM: u64 = 7_001;

    /// One scripted response: either a listing page or a raw status body.
    enum Scripted {
        Page(ThreadsListing),
        Status(u16, String),
    }

    /// Scripted listing responses consumed in request order; `recorded`
    /// captures the `before` query value of each request (decoded, `None`
    /// for the first page) so tests can assert the continuation chain.
    struct ArchiveMock {
        recorded: Arc<Mutex<Vec<Option<String>>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl ArchiveMock {
        async fn start(pages: Vec<ThreadsListing>) -> (Self, String) {
            Self::start_script(pages.into_iter().map(Scripted::Page).collect()).await
        }

        async fn start_script(script: Vec<Scripted>) -> (Self, String) {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
            let origin = format!("http://{}", listener.local_addr().expect("addr"));
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
            let (rec, scr) = (recorded.clone(), script.clone());
            let handle = tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let (rec, scr) = (rec.clone(), scr.clone());
                    tokio::spawn(async move {
                        let Some(path) = read_get(&mut stream).await else {
                            return;
                        };
                        rec.lock()
                            .expect("recorded")
                            .push(decode_before_query(&path));
                        let next =
                            scr.lock()
                                .expect("script")
                                .pop_front()
                                .unwrap_or(Scripted::Page(ThreadsListing {
                                    has_more: Some(false),
                                    members: Vec::new(),
                                    threads: Vec::new(),
                                }));
                        let (status, reason, body) = match next {
                            Scripted::Page(listing) => (
                                200,
                                "OK",
                                serde_json::to_string(&listing).expect("fixture serializes"),
                            ),
                            Scripted::Status(status, body) => (status, reason_for(status), body),
                        };
                        let head = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(body.as_bytes()).await;
                    });
                }
            });
            (Self { recorded, handle }, origin)
        }

        fn befores(&self) -> Vec<Option<String>> {
            self.recorded.lock().expect("recorded").clone()
        }

        fn shutdown(self) {
            self.handle.abort();
        }
    }

    fn reason_for(status: u16) -> &'static str {
        match status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Internal Server Error",
        }
    }

    async fn read_get(stream: &mut TcpStream) -> Option<String> {
        let mut buf = Vec::with_capacity(1024);
        let header_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            if buf.len() > 64 * 1024 {
                return None;
            }
            if stream.read_buf(&mut buf).await.ok()? == 0 {
                return None;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let request_line = head.split("\r\n").next()?;
        Some(request_line.split(' ').nth(1)?.to_owned())
    }

    /// Decode the `before` query value the client percent-encodes (`+` stays
    /// literal, `%XX` decodes), or `None` when the request has no cursor yet.
    fn decode_before_query(path: &str) -> Option<String> {
        let query = path.split_once('?')?.1;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=')?;
            if k == "before" {
                return Some(percent_decode(v));
            }
        }
        None
    }

    fn percent_decode(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let hex_pair = bytes[i] == b'%'
                && i + 2 < bytes.len()
                && hex_val(bytes[i + 1]).is_some()
                && hex_val(bytes[i + 2]).is_some();
            if hex_pair {
                let h = hex_val(bytes[i + 1]).expect("checked above");
                let l = hex_val(bytes[i + 2]).expect("checked above");
                out.push(h << 4 | l);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("cursor is ASCII ISO-8601")
    }

    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    /// One synthetic archived thread: snowflake `id`, archived at
    /// `2024-01-01T00:{mm}:{ss}.000000+00:00`.
    fn thread(id: u64, minute: u64, second: u64) -> Channel {
        let ts = twilight_model::util::Timestamp::from_str(&format!(
            "2024-01-01T00:{minute:02}:{second:02}.000000+00:00"
        ))
        .expect("fixture timestamp");
        Channel {
            application_id: None,
            applied_tags: None,
            available_tags: None,
            bitrate: None,
            default_auto_archive_duration: None,
            default_forum_layout: None,
            default_reaction_emoji: None,
            default_sort_order: None,
            default_thread_rate_limit_per_user: None,
            flags: None,
            guild_id: None,
            icon: None,
            id: Id::new(id),
            invitable: None,
            kind: ChannelType::PublicThread,
            last_message_id: None,
            last_pin_timestamp: None,
            managed: None,
            member: None,
            member_count: None,
            message_count: None,
            name: None,
            newly_created: None,
            nsfw: None,
            owner_id: None,
            parent_id: None,
            permission_overwrites: None,
            position: None,
            rate_limit_per_user: None,
            recipients: None,
            rtc_region: None,
            thread_metadata: Some(ThreadMetadata {
                archived: true,
                auto_archive_duration: AutoArchiveDuration::Day,
                archive_timestamp: ts,
                create_timestamp: None,
                invitable: None,
                locked: false,
            }),
            topic: None,
            user_limit: None,
            video_quality_mode: None,
        }
    }

    fn page(threads: Vec<Channel>, has_more: bool) -> ThreadsListing {
        ThreadsListing {
            has_more: Some(has_more),
            members: Vec::new(),
            threads,
        }
    }

    /// 130 newest-first threads (`id` 130 newest … 1 oldest, one per minute).
    fn forum_130() -> Vec<Channel> {
        (1u64..=130)
            .rev()
            .map(|id| thread(9_000_000_000_000_000_000 + id, id / 60, id % 60))
            .collect()
    }

    fn client_for(origin: &str) -> RestClient {
        RestClient::with_proxy("archived-test-token".to_owned(), Some(origin.to_owned()))
    }

    fn forum_id() -> Id<ChannelMarker> {
        Id::new(FORUM)
    }

    #[tokio::test]
    async fn discovers_all_130_threads_across_two_pages() {
        let all = forum_130();
        let cursor = all[99]
            .thread_metadata
            .as_ref()
            .expect("fixture metadata")
            .archive_timestamp
            .iso_8601()
            .to_string();
        let (mock, origin) = ArchiveMock::start(vec![
            page(all[..100].to_vec(), true),
            page(all[100..].to_vec(), false),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete(), "two full pages complete");
        let threads = outcome.threads();
        assert_eq!(threads.len(), 130, "every thread discovered");
        let mut ids: Vec<u64> = threads.iter().map(|t| t.id.get()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 130, "each thread discovered exactly once");
        let befores = mock.befores();
        assert_eq!(befores.len(), 2, "one continuation request");
        assert_eq!(befores[0], None, "first page has no cursor");
        assert_eq!(
            befores[1],
            Some(cursor),
            "second page continues past page one"
        );
        mock.shutdown();
    }

    #[tokio::test]
    async fn empty_final_page_terminates_complete() {
        let (mock, origin) = ArchiveMock::start(vec![page(Vec::new(), false)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete());
        assert!(outcome.threads().is_empty());
        assert_eq!(mock.befores().len(), 1, "no continuation after empty page");
        mock.shutdown();
    }

    #[tokio::test]
    async fn cleared_has_more_terminates_complete() {
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start(vec![page(all[..50].to_vec(), false)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete());
        assert_eq!(outcome.threads().len(), 50);
        assert_eq!(
            mock.befores().len(),
            1,
            "no continuation after has_more=false"
        );
        mock.shutdown();
    }

    #[tokio::test]
    async fn repeated_continuation_reports_incomplete() {
        let batch = forum_130()[..100].to_vec();
        let (mock, origin) =
            ArchiveMock::start(vec![page(batch.clone(), true), page(batch.clone(), true)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(!outcome.is_complete(), "repeating cursor is not complete");
        assert_eq!(
            outcome,
            ArchivedThreadsOutcome::Incomplete {
                threads: outcome.threads().to_vec(),
                reason: ArchiveIncompleteReason::NoProgress,
            }
        );
        assert_eq!(outcome.threads().len(), 100, "deduped by snowflake");
        assert_eq!(mock.befores().len(), 2, "stops instead of looping");
        mock.shutdown();
    }

    #[tokio::test]
    async fn page_budget_reports_incomplete() {
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start(vec![
            page(all[..100].to_vec(), true),
            page(all[100..].to_vec(), true),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), 1)
            .await
            .expect("walk succeeds");

        assert_eq!(
            outcome,
            Some(ArchivedThreadsOutcome::Incomplete {
                threads: all[..100].to_vec(),
                reason: ArchiveIncompleteReason::PageBudget,
            })
        );
        assert_eq!(mock.befores().len(), 1, "budget stops the walk");
        mock.shutdown();
    }

    #[tokio::test]
    async fn refused_second_page_ends_walk_incomplete_with_threads_seen() {
        // First page readable with `has_more`, second page 403: the walk
        // ends `Incomplete` with `AccessLost` and page one's threads, so
        // the caller records the truncation instead of reading a partial
        // walk as complete.
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start_script(vec![
            Scripted::Page(page(all[..100].to_vec(), true)),
            Scripted::Status(403, "{}".to_owned()),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("partial walk is not an error")
            .expect("first page was readable");

        assert_eq!(
            outcome,
            ArchivedThreadsOutcome::Incomplete {
                threads: all[..100].to_vec(),
                reason: ArchiveIncompleteReason::AccessLost,
            }
        );
        assert!(!outcome.is_complete());
        assert_eq!(mock.befores().len(), 2, "walked until refused");
        mock.shutdown();
    }

    #[tokio::test]
    async fn unreadable_forum_reports_no_listing_like_before() {
        // The walk translates a 403 refusal into its legacy walk-end; the
        // legacy consumer sees exactly what the old one-page call returned
        // for an unreadable forum.
        let (mock, origin) = ArchiveMock::start_script(vec![
            Scripted::Status(403, "{}".to_owned()),
            Scripted::Status(403, "{}".to_owned()),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("unreadable forum is not an error");
        assert!(
            outcome.is_none(),
            "unreadable first page stays None like the legacy call"
        );
        assert_eq!(
            rest.public_archived_threads(forum_id())
                .await
                .expect("compat helper"),
            None,
            "compat helper preserves the legacy None contract"
        );
        assert_eq!(
            mock.befores().len(),
            2,
            "one discovery page plus one compat call"
        );
        mock.shutdown();
    }

    #[tokio::test]
    async fn non_refusal_errors_propagate_from_walk() {
        // Only lost access ends the walk: a corrupt page (like a spent 5xx
        // budget) is an error, never a silent walk-end.
        let (mock, origin) =
            ArchiveMock::start_script(vec![Scripted::Status(200, "not json".to_owned())]).await;
        let rest = client_for(&origin);

        let error = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect_err("a corrupt page fails the walk");
        assert!(
            error.to_string().contains("unreadable"),
            "unexpected error: {error:?}"
        );
        mock.shutdown();
    }

    /// A page whose headers never arrive must fail at the 30 s page budget on
    /// every transport call site (`exec_list`, `exec_one`, `delete_channel`):
    /// the absolute deadline lives in the transport, so reverting any of
    /// those sites to `None` parks the caller instead of returning. The clock
    /// is paused so the three budgets elapse without wall time; the stall can
    /// never win.
    #[tokio::test(start_paused = true)]
    async fn stalled_reads_time_out_at_page_budget() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use twilight_model::id::{
            marker::{ChannelMarker, GuildMarker},
            Id,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stall");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let accepted = Arc::new(AtomicU32::new(0));
        let handle = {
            let accepted = Arc::clone(&accepted);
            tokio::spawn(async move {
                // One stalled connection per call site below. Each connection
                // is stalled on its own task so the accept loop stays
                // responsive: a sequential sleep past the budget would hold
                // the loop while the paused clock fires the caller's timeout,
                // and later call sites would time out before being accepted.
                for _ in 0..3 {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    accepted.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let mut buf = Vec::with_capacity(1024);
                        let mut chunk = [0u8; 1024];
                        while let Ok(n) = stream.read(&mut chunk).await {
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(600)).await;
                    });
                }
            })
        };
        let rest = client_for(&origin);
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            rest.fetch_all_members(Id::<GuildMarker>::new(2222)),
        )
        .await
        .expect("a stalled member page must not outlive the page budget");
        assert!(result.is_err(), "a stalled page is an error: {result:?}");
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            rest.guild(Id::<GuildMarker>::new(2222)),
        )
        .await
        .expect("a stalled guild read must not outlive the page budget");
        assert!(result.is_err(), "a stalled read is an error: {result:?}");
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            rest.delete_channel(Id::<ChannelMarker>::new(3333), "stall test"),
        )
        .await
        .expect("a stalled delete must not outlive the page budget");
        assert!(result.is_err(), "a stalled delete is an error: {result:?}");
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            3,
            "one stalled wire request per call site"
        );
        handle.abort();
    }

    /// Threat-model F6 refusal pins for the operator-tool admission pool: a
    /// remote `sslmode=disable` URL (and the other refusal cases) fails with
    /// a fixed string before SQLx parses the URL or opens a socket, with no
    /// URL part in the error. Hermetic: every case is refused, so no
    /// connection is attempted and no database is needed. The log-silence half
    /// of the proof lives in
    /// `crates/cutover/tests/admission_configuration.rs`, where the capture
    /// helper is already wired.
    #[tokio::test]
    async fn admission_tls_fence_refuses_plaintext_and_wrong_hosts() {
        use two_bot_core::database_tls::TlsPolicy;
        let cases = [
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=disable",
                TlsPolicy::Required,
                "database sslmode does not require TLS",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=prefer",
                TlsPolicy::Required,
                "database sslmode does not require TLS",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db",
                TlsPolicy::Required,
                "database URL must set sslmode under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=verify-full",
                TlsPolicy::Required,
                "local database host is refused under the required TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=verify-full",
                TlsPolicy::LocalOnly,
                "remote database host is refused under the local-only TLS policy",
            ),
            (
                "postgres://fixture-user:fixture-db-password@fixture-host/fixture-db?sslmode=fixture-mode",
                TlsPolicy::LocalOnly,
                "unsupported database sslmode",
            ),
        ];
        for (url, policy, expected) in cases {
            let error = admission_connect_options(url, policy).unwrap_err();
            assert_eq!(error, expected, "{url}");
            assert!(!error.contains("fixture"), "TLS refusal echoed the URL");
            // Same through the client constructor: refused before the pool
            // opens, so no socket and no database is needed.
            let error =
                RestClient::from_url_with_tls("fixture-token".to_owned(), None, url, policy)
                    .await
                    .unwrap_err();
            let shown = error.to_string();
            assert_eq!(shown, format!("discord send refused: {expected}"));
            assert!(!shown.contains("fixture"), "TLS refusal echoed the URL");
            let debug = format!("{error:?}");
            assert!(!debug.contains("fixture"), "TLS refusal echoed the URL");
        }
    }

    /// Threat-model F6 happy path: a fenced fixture builds fenced options
    /// (effective SQLx mode applied) and a lazy pool with no socket, so no
    /// database is needed.
    #[tokio::test]
    async fn admission_tls_fence_permits_fenced_fixture() {
        use two_bot_core::database_tls::TlsPolicy;
        // `LocalOnly` loopback keeps the URL mode as written.
        let options = admission_connect_options(
            "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable",
            TlsPolicy::LocalOnly,
        )
        .unwrap();
        assert!(matches!(
            options.get_ssl_mode(),
            sqlx::postgres::PgSslMode::Disable
        ));
        // `Required` upgrades a remote `require` URL to `verify-full`.
        let options = admission_connect_options(
            "postgres://fixture-user:fixture-db-password@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=require",
            TlsPolicy::Required,
        )
        .unwrap();
        assert!(matches!(
            options.get_ssl_mode(),
            sqlx::postgres::PgSslMode::VerifyFull
        ));
        // A lazy pool from fenced options opens no socket.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options);
        assert_eq!(pool.size(), 0);
        pool.close().await;
    }

    /// Loopback member list serving one scripted HTTP status per request
    /// (consumed in order; repeats the last once the script is spent), so
    /// failure-path tests never touch Discord.
    struct StatusMock {
        handle: tokio::task::JoinHandle<()>,
    }

    impl StatusMock {
        async fn start(mut script: Vec<u16>) -> (Self, String) {
            if script.is_empty() {
                script.push(200);
            }
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
            let origin = format!("http://{}", listener.local_addr().expect("addr"));
            let handle = tokio::spawn(async move {
                let mut script = script.into_iter();
                let mut last = 200u16;
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = Vec::with_capacity(1024);
                    let header_end = loop {
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos;
                        }
                        if buf.len() > 64 * 1024 {
                            break usize::MAX;
                        }
                        match stream.read_buf(&mut buf).await {
                            Ok(0) => break usize::MAX,
                            Ok(_) => continue,
                            Err(_) => break usize::MAX,
                        }
                    };
                    if header_end == usize::MAX {
                        continue;
                    }
                    if let Some(next) = script.next() {
                        last = next;
                    }
                    let (reason, body) = match last {
                        200 => ("OK", "[]".to_owned()),
                        403 => ("Forbidden", r#"{"message": "403"}"#.to_owned()),
                        404 => ("Not Found", r#"{"message": "404"}"#.to_owned()),
                        _ => ("Service Unavailable", r#"{"retry_after": 1}"#.to_owned()),
                    };
                    let head = format!(
                        "HTTP/1.1 {last} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                }
            });
            (Self { handle }, origin)
        }

        fn shutdown(self) {
            self.handle.abort();
        }
    }

    /// Paperclip Review #927: a failed member page is an error, never an
    /// empty page. A 403 refuses at once; a 5xx that is still failing after
    /// the last retry surfaces as retry-exhausted. Either way the caller —
    /// and the catch-up walker — must not read it as end of list.
    #[tokio::test]
    async fn failed_member_pages_are_errors_not_empty_pages() {
        let (mock, origin) = StatusMock::start(vec![403]).await;
        let rest = client_for(&origin);
        let err = rest
            .fetch_members_page_json(Id::new(2222), 1000, None)
            .await
            .expect_err("a 403 member page is an error, not end of list");
        assert!(
            err.to_string().contains("unreadable"),
            "unexpected error: {err:?}"
        );
        mock.shutdown();

        // Five 503s (initial attempt plus four retries) exhaust the page
        // budget: the sixth scripted status is never reached.
        let (mock, origin) = StatusMock::start(vec![503, 503, 503, 503, 503, 200]).await;
        let rest = client_for(&origin);
        let err = rest
            .fetch_members_page_json(Id::new(2222), 1000, None)
            .await
            .expect_err("an exhausted 5xx member page is an error, not end of list");
        assert!(
            err.to_string().contains("unreadable"),
            "unexpected error: {err:?}"
        );
        mock.shutdown();
    }

    #[test]
    fn jittered_backoff_stays_within_base_and_replays_per_seed() {
        for attempt in 0..5u32 {
            let base = backoff_duration(attempt);
            for seed in 0..8u64 {
                let sample = backoff_duration_jittered(attempt, seed);
                assert!(
                    sample >= Duration::from_millis(1) && sample <= base,
                    "attempt {attempt} seed {seed}: {sample:?} outside [1 ms, {base:?}]"
                );
            }
        }
        assert_eq!(
            backoff_duration_jittered(3, 42),
            backoff_duration_jittered(3, 42)
        );
    }

    #[test]
    fn jittered_backoff_spreads_across_seeds() {
        use std::collections::HashSet;
        // Attempt 3 has a 4 s base: concurrent scans must not retry in phase.
        let distinct: HashSet<Duration> = (0..32u64)
            .map(|seed| backoff_duration_jittered(3, seed))
            .collect();
        assert!(
            distinct.len() >= 8,
            "only {} distinct delays across 32 seeds",
            distinct.len()
        );
    }

    #[test]
    fn jittered_backoff_never_exceeds_the_saturated_cap() {
        let cap = Duration::from_secs(512);
        assert_eq!(backoff_duration(u32::MAX), cap);
        for seed in [0u64, 1, 42] {
            let sample = backoff_duration_jittered(u32::MAX, seed);
            assert!(
                sample >= Duration::from_millis(1) && sample <= cap,
                "seed {seed}: {sample:?} outside [1 ms, {cap:?}]"
            );
        }
    }
}
