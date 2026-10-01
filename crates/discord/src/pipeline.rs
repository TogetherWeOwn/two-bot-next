//! Gateway → funnel pipeline (S3): the twilight half of the `registerHandlers`
//! port (two-bot `src/discord/client.ts`, scoped by `docs/parity.md` §3).
//!
//! One [`Pipeline`] owns the cache, the invite snapshot + expected-join
//! attribution inputs, the level/facts seams, and the core [`FunnelHandlers`].
//! `handle()` takes one twilight [`Event`], reads pre-update state where the
//! transition matters (member pending, voice channel), updates the cache, then
//! drives the framework-free handlers with legacy semantics:
//!
//! * ready/resumed → drop open voice sessions (TOG-6123). READY means a fresh
//!   session here: twilight parses RESUMED as its own variant, and the mock
//!   gateway (like Discord) sends READY only after (re-)identify.
//! * guild-create (available) → seed invite snapshot + vanity flag.
//! * join: snapshot invites → expected-join note wins → invite-diff attribute
//!   → `on_join` → instant `on_gate_cleared` when `!pending`.
//! * `MemberUpdate` pending true→false → `on_gate_cleared` (role/nickname
//!   diffs are S5 audit, not funnel — parity matrix §3).
//! * remove → `on_leave` (closes open voice first, TOG-6122).
//! * message (guild only; DMs dropped) → `on_message`; automod rejection is
//!   an S4 concern and arrives as `capture_only` from the automod seam.
//! * voice: channel-change only; per-member serial chain (TOG-5981); end(A)
//!   then start(B) at one timestamp; bots resolve the flag from the frame.
//! * `InviteCreate` → seed the code at its current uses (0 live) so the next
//!   join's growth diff is measured against a baseline instead of treating
//!   the whole counter as new.
//!
//! Everything Discord-adjacent in tests runs against `tools/mock-discord`
//! style payloads — real twilight `Event` values built in-memory — never the
//! production guild or tokens.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use twilight_cache_inmemory::{DefaultInMemoryCache, InMemoryCache};
use twilight_model::gateway::event::Event;
use twilight_model::util::Timestamp;
use two_bot_core::{
    ChannelClass, ExpectedJoins, FactsSink, FunnelHandlers, FunnelStore, GateClearedInput,
    InviteSnapshotStore, InviteState, InviteTracker, JoinInput, LevelingHook, MessageInput,
    Snowflake, VoiceInput,
};

/// Legacy `occurred_at` shape: millis precision with a `Z` suffix
/// (`new Date().toISOString()`). Twilight renders micros + `+00:00`;
/// row bytes must match legacy, so every frame stamp converts here.
#[must_use]
pub fn legacy_stamp(ts: Timestamp) -> String {
    two_bot_core::format_iso_millis(ts.as_micros().div_euclid(1000))
}

use crate::intents::cache_resource_types;

fn user_id_key(u: u64) -> twilight_model::id::Id<twilight_model::id::marker::UserMarker> {
    twilight_model::id::Id::new(u)
}

fn guild_id_key(u: u64) -> twilight_model::id::Id<twilight_model::id::marker::GuildMarker> {
    twilight_model::id::Id::new(u)
}

/// Invite fetching seam: the live pipeline reads counters over REST
/// (`twilight-http`, S6 wires the client); the replay/mock pipeline serves
/// scripted counters. `None` is a failed read — joins still get recorded,
/// source `unknown` (legacy `snapshotInvites` catch path).
pub trait InviteSource: Send + Sync {
    fn current(&self, guild_id: Snowflake) -> Option<Vec<InviteState>>;
}

/// Invite fetching unavailable (default; joins attribute `unknown`/`vanity`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoInvites;

impl InviteSource for NoInvites {
    fn current(&self, _guild_id: Snowflake) -> Option<Vec<InviteState>> {
        None
    }
}

/// Scripted counters for the replay harness: each guild maps to the sequence
/// of snapshots returned by successive reads. A guild with no queue reads as
/// a failed fetch (`None`); queue an explicit empty vec for "no invites".
#[derive(Debug, Default)]
pub struct ScriptedInvites {
    inner: Mutex<HashMap<Snowflake, Vec<Vec<InviteState>>>>,
}

impl ScriptedInvites {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the next snapshot a read for `guild_id` will return.
    pub fn push(&self, guild_id: Snowflake, snapshot: Vec<InviteState>) {
        self.inner
            .lock()
            .expect("invites lock")
            .entry(guild_id)
            .or_default()
            .push(snapshot);
    }
}

impl InviteSource for ScriptedInvites {
    fn current(&self, guild_id: Snowflake) -> Option<Vec<InviteState>> {
        let mut inner = self.inner.lock().expect("invites lock");
        match inner.get_mut(&guild_id) {
            Some(queue) if !queue.is_empty() => Some(queue.remove(0)),
            _ => None,
        }
    }
}

/// `InviteSnapshotStore` over a mutex map (pipeline-owned; S6 replaces it
/// with the sqlx `invite_snapshots` table).
#[derive(Debug, Default)]
pub struct PipelineSnapshots {
    inner: Mutex<HashMap<Snowflake, HashMap<String, InviteState>>>,
}

impl PipelineSnapshots {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InviteSnapshotStore for PipelineSnapshots {
    fn load(&self, guild_id: Snowflake) -> Vec<InviteState> {
        self.inner
            .lock()
            .expect("snapshot lock")
            .get(&guild_id)
            .map_or_else(Vec::new, |t| t.values().cloned().collect())
    }

    fn store_all(&self, guild_id: Snowflake, states: &[InviteState]) {
        let mut tables = self.inner.lock().expect("snapshot lock");
        let table = tables.entry(guild_id).or_default();
        for s in states {
            table.insert(s.code.clone(), s.clone());
        }
    }

    fn delete_missing(&self, guild_id: Snowflake, live_codes: &HashSet<String>) {
        if let Some(table) = self.inner.lock().expect("snapshot lock").get_mut(&guild_id) {
            table.retain(|code, _| live_codes.contains(code));
        }
    }
}

/// Optional community channel classification for message attribution
/// (S5 scorecard owns the real sets; default is `Other` everywhere).
pub trait ChannelClassifier: Send + Sync {
    fn classify(&self, channel_id: Snowflake) -> ChannelClass;
}

/// Default classifier: every channel is `Other`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoClassification;

impl ChannelClassifier for NoClassification {
    fn classify(&self, _channel_id: Snowflake) -> ChannelClass {
        ChannelClass::Other
    }
}

/// Per-member serial chains for voice frames (TOG-5981): the shard loop is
/// single-threaded per shard, but S4 slow paths (automod, audit REST) may
/// yield between a move's leave and join. The mutex per member keeps the
/// end(A)→start(B) pair atomic against any interleaved frame for the same
/// member; a stuck write for A never stalls B. The guard is a blocking
/// `std` mutex: if `handle` ever awaits inside the voice critical section,
/// switch this to an async mutex first.
type VoiceChainKey = (Snowflake, Snowflake);
type VoiceChainMap = HashMap<VoiceChainKey, Arc<Mutex<()>>>;

#[derive(Debug, Default)]
struct VoiceChains {
    inner: Mutex<VoiceChainMap>,
}

impl VoiceChains {
    fn lock_for(&self, guild_id: Snowflake, member_id: Snowflake) -> Arc<Mutex<()>> {
        self.inner
            .lock()
            .expect("chains lock")
            .entry((guild_id, member_id))
            .or_default()
            .clone()
    }
}

/// Eligibility decisions supplied by the upstream message acceptance path.
#[derive(Debug, Default, Clone, Copy)]
pub struct MessageEligibility {
    pub is_staff_automation: bool,
    pub capture_only: bool,
}

/// The S3 gateway pipeline. `S`/`L`/`F` are the core seams; `I` serves invite
/// counters; `C` classifies channels. Share via `Arc` between the shard
/// runner and the HTTP layer (snapshot reads, health).
pub struct Pipeline<
    S = two_bot_core::MemStore,
    L = two_bot_core::NoopLeveling,
    F = two_bot_core::NoopFacts,
    I = NoInvites,
    C = NoClassification,
> {
    cache: InMemoryCache,
    handlers: FunnelHandlers<S, L, F>,
    invites: InviteTracker<PipelineSnapshots>,
    invite_source: I,
    expected_joins: Mutex<ExpectedJoins>,
    classifier: C,
    voice_chains: VoiceChains,
    /// Guild IDs whose invites carry a vanity URL (for `vanity` attribution).
    vanity_guilds: Mutex<HashSet<Snowflake>>,
}

impl<S, L, F, I, C> std::fmt::Debug for Pipeline<S, L, F, I, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline").finish_non_exhaustive()
    }
}

impl<S: FunnelStore, L: LevelingHook, F: FactsSink, I: InviteSource, C: ChannelClassifier>
    Pipeline<S, L, F, I, C>
{
    /// Build a pipeline over the given seams.
    pub fn new(
        store: S,
        leveling: Option<L>,
        facts: Option<F>,
        invite_source: I,
        classifier: C,
    ) -> Self {
        Self {
            cache: InMemoryCache::builder()
                .resource_types(cache_resource_types())
                .build(),
            handlers: FunnelHandlers::new(store, leveling, facts),
            invites: InviteTracker::new(PipelineSnapshots::new()),
            invite_source,
            expected_joins: Mutex::new(ExpectedJoins::new()),
            classifier,
            voice_chains: VoiceChains::default(),
            vanity_guilds: Mutex::new(HashSet::new()),
        }
    }

    /// Access the core handlers (tracker reads, replay assertions).
    #[must_use]
    pub fn handlers(&self) -> &FunnelHandlers<S, L, F> {
        &self.handlers
    }

    /// Access the cache (shard runner updates, tests seed).
    #[must_use]
    pub fn cache(&self) -> &InMemoryCache {
        &self.cache
    }

    /// Access the invite source (replay tests queue scripted snapshots).
    #[must_use]
    pub fn invite_source(&self) -> &I {
        &self.invite_source
    }

    /// Mark a guild as vanity-URL-holding (from `GuildCreate` features).
    pub fn set_guild_vanity(&self, guild_id: Snowflake, has_vanity: bool) {
        let mut set = self.vanity_guilds.lock().expect("vanity lock");
        if has_vanity {
            set.insert(guild_id);
        } else {
            set.remove(&guild_id);
        }
    }

    /// Stage a one-click expectation BEFORE the add call (see
    /// [`ExpectedJoins::expect`]).
    pub fn expect_join(&self, guild_id: Snowflake, member_id: Snowflake, source: String) {
        self.expected_joins
            .lock()
            .expect("expected lock")
            .expect(guild_id, member_id, source);
    }

    /// Take the connect-time invite snapshot (mirrors the legacy `ClientReady`
    /// handler: per-guild baseline so the first join's growth diff measures
    /// against witnessed counters, not an empty table).
    pub fn prime_invite_snapshot(&self, guild_id: Snowflake) -> Vec<String> {
        self.snapshot_invites(guild_id, &two_bot_core::now_iso())
    }

    /// Snapshot invite counters for a guild; returns the codes that grew.
    /// A failed read (`None`) keeps the old snapshot and returns empty, so
    /// the join still records with source `unknown`.
    fn snapshot_invites(&self, guild_id: Snowflake, observed_at: &str) -> Vec<String> {
        match self.invite_source.current(guild_id) {
            Some(current) => {
                let grew = self.invites.diff_and_store(guild_id, &current);
                self.handlers.store().stage_invite_snapshot(
                    two_bot_core::gateway_funnel::InviteSnapshotWrite {
                        guild_id,
                        states: current,
                        observed_at: observed_at.to_owned(),
                        replace_all: true,
                    },
                );
                grew
            }
            None => Vec::new(),
        }
    }

    /// A newly created code is an upsert, not a full REST snapshot.
    fn seed_invite_code(&self, guild_id: Snowflake, state: InviteState, observed_at: &str) {
        self.invites.seed(guild_id, state.clone());
        self.handlers.store().stage_invite_snapshot(
            two_bot_core::gateway_funnel::InviteSnapshotWrite {
                guild_id,
                states: vec![state],
                observed_at: observed_at.to_owned(),
                replace_all: false,
            },
        );
    }

    /// Drop open voice sessions on (re)connect. Both recovery paths (TOG-6123):
    /// RESUMED after a successful resume, READY after a fresh session.
    fn drop_sessions_on_reconnect(&self) {
        self.handlers
            .voice_sessions
            .lock()
            .expect("voice lock")
            .clear();
    }

    /// Drive one gateway event through the funnel. Reads pre-update state
    /// where the transition matters (member pending, voice channel), updates
    /// the cache, then calls the framework-free handlers.
    pub fn handle(&self, event: &Event) {
        self.handle_at(event, &two_bot_core::now_iso());
    }

    /// Replay with an explicit observation clock for frames without timestamps.
    /// Join/message payload timestamps still take precedence over this clock.
    pub fn handle_at(&self, event: &Event, observed_at: &str) {
        self.handle_at_with_eligibility(event, observed_at, MessageEligibility::default());
    }

    /// The ordered async bridge supplies one processing instant for both halves
    /// of a voice move and carries the existing automod/staff eligibility gates.
    pub fn handle_at_with_eligibility(
        &self,
        event: &Event,
        observed_at: &str,
        eligibility: MessageEligibility,
    ) {
        match event {
            // Fresh session after (re-)identify: first connect starts empty
            // (no-op); a reconnect's open state is unproven and dropped.
            // RESUMED and READY never double-drop (TOG-6123): twilight parses
            // RESUMED as its own variant, READY only follows re-identify.
            Event::Ready(_) | Event::Resumed => {
                self.drop_sessions_on_reconnect();
                self.cache.update(event);
            }
            Event::GuildCreate(gc) => {
                if let twilight_model::gateway::payload::incoming::GuildCreate::Available(guild) =
                    gc.as_ref()
                {
                    let gid = guild.id.get();
                    self.set_guild_vanity(gid, guild.vanity_url_code.is_some());
                    self.snapshot_invites(gid, observed_at);
                }
                self.cache.update(event);
            }
            Event::MemberAdd(add) => {
                let guild_id = add.guild_id.get();
                let member_id = add.user.id.get();
                // Snapshot regardless of arrival path so counters stay current
                // for the next organic join.
                let grew = self.snapshot_invites(guild_id, observed_at);
                // The web path's expected join beats the invite diff: a code
                // that grew in the same window belongs to some other join.
                let expected = self
                    .expected_joins
                    .lock()
                    .expect("expected lock")
                    .consume(guild_id, member_id);
                let has_vanity = self
                    .vanity_guilds
                    .lock()
                    .expect("vanity lock")
                    .contains(&guild_id);
                let source = expected.unwrap_or_else(|| self.invites.attribute(&grew, has_vanity));
                let inviter_id = if grew.len() == 1 {
                    self.invites.inviter_for(guild_id, &grew[0])
                } else {
                    None
                };
                let joined_at = Some(
                    add.member
                        .joined_at
                        .map_or_else(|| observed_at.to_owned(), legacy_stamp),
                );
                let source_event_id = format!(
                    "{guild_id}:{member_id}:{}",
                    joined_at.as_deref().unwrap_or("observed")
                );
                let is_bot = add.user.bot;
                self.cache.update(event);
                self.handlers.on_join(JoinInput {
                    guild_id,
                    member_id,
                    is_bot,
                    source,
                    occurred_at: joined_at.clone(),
                    inviter_id,
                    source_event_id: Some(source_event_id),
                });
                // Arrived with the gate already cleared (accepted on the
                // invite screen): record it here too, or the fastest members
                // go missing from the numerator.
                if !add.member.pending {
                    self.handlers.on_gate_cleared(GateClearedInput {
                        guild_id,
                        member_id,
                        is_bot,
                        occurred_at: joined_at,
                        source: None,
                    });
                }
            }
            Event::MemberUpdate(update) => {
                let guild_id = update.guild_id.get();
                let member_id = update.user.id.get();
                // Read the PRE-update pending flag: the cache still holds the
                // old member (update lands below), mirroring discord.js's
                // (oldMember, newMember) pair. Members never seen (no
                // GUILD_MEMBERS intent delivery yet) read as pending so a
                // first sighting of a cleared member still clears the gate.
                let was_pending = self
                    .cache
                    .member(guild_id_key(guild_id), user_id_key(member_id))
                    .map(|m| m.pending())
                    .unwrap_or(true);
                let is_bot = update.user.bot;
                self.cache.update(event);
                if was_pending && !update.pending {
                    self.handlers.on_gate_cleared(GateClearedInput {
                        guild_id,
                        member_id,
                        is_bot,
                        occurred_at: Some(observed_at.to_owned()),
                        source: None,
                    });
                }
            }
            Event::MemberRemove(remove) => {
                let guild_id = remove.guild_id.get();
                let member_id = remove.user.id.get();
                let is_bot = remove.user.bot;
                self.cache.update(event);
                self.handlers.on_leave(
                    guild_id,
                    member_id,
                    Some(observed_at.to_owned()),
                    Some(is_bot),
                );
            }
            Event::MessageCreate(msg) => {
                let Some(guild_id) = msg.guild_id.map(|g| g.get()) else {
                    return; // DMs dropped.
                };
                let channel_id = msg.channel_id.get();
                let input = MessageInput {
                    guild_id,
                    member_id: msg.author.id.get(),
                    is_bot: msg.author.bot,
                    message_id: Some(msg.id.get().to_string()),
                    webhook_id: msg.webhook_id.map(|w| w.get()),
                    is_staff_automation: eligibility.is_staff_automation,
                    channel_id,
                    channel_class: self.classifier.classify(channel_id),
                    capture_only: eligibility.capture_only,
                    occurred_at: Some(legacy_stamp(msg.timestamp)),
                };
                self.cache.update(event);
                self.handlers.on_message(input);
            }
            Event::VoiceStateUpdate(update) => {
                let Some(guild_id) = update.guild_id.map(|g| g.get()) else {
                    return;
                };
                let member_id = update.user_id.get();
                // Pre-update channel from the cache: mute/deafen/camera frames
                // carry no channel change and are not session boundaries.
                let old_channel = self
                    .cache
                    .voice_state(user_id_key(member_id), guild_id_key(guild_id))
                    .map(|v| v.channel_id().get());
                let new_channel = update.channel_id.map(|c| c.get());
                if old_channel == new_channel {
                    self.cache.update(event);
                    return;
                }
                // Bot flag: the frame's member, else non-bot. Member-less
                // frames are rare (uncached user in a voice frame); legacy
                // treats a missing member as non-bot, so the fallback is a
                // plain `false` with no cache read.
                let is_bot = update.member.as_ref().is_some_and(|m| m.user.bot);
                self.cache.update(event);
                // Per-member serial chain (TOG-5981): hold the member's
                // mutex across end(A)→start(B) so the pair stays atomic for
                // this member. One timestamp for both halves so a move reads
                // as one instant, not a gap.
                let chain = self.voice_chains.lock_for(guild_id, member_id);
                let _guard = chain.lock().expect("voice chain");
                let at = observed_at.to_owned();
                if let Some(old) = old_channel {
                    self.handlers.on_voice_leave(VoiceInput {
                        guild_id,
                        member_id,
                        is_bot,
                        channel_id: old,
                        occurred_at: Some(at.clone()),
                    });
                }
                if let Some(channel) = new_channel {
                    self.handlers.on_voice_join(VoiceInput {
                        guild_id,
                        member_id,
                        is_bot,
                        channel_id: channel,
                        occurred_at: Some(at.clone()),
                    });
                }
            }
            Event::InviteCreate(invite) => {
                self.seed_invite_code(
                    invite.guild_id.get(),
                    InviteState {
                        code: invite.code.clone(),
                        uses: u64::from(invite.uses),
                        inviter_id: invite.inviter.as_ref().map(|user| user.id.get()),
                        channel_id: Some(invite.channel_id.get()),
                    },
                    observed_at,
                );
                self.cache.update(event);
            }
            // Connection lifecycle and S4/S5 surfaces: no funnel row.
            // (Reactions, audit-log entries, interactions, bans, message
            // updates/deletes are S4/S5 — parity matrix §§1–3. This match is
            // exhaustive-by-construction: new twilight variants land here and
            // must be triaged, never silently swallowed.)
            _ => {
                self.cache.update(event);
            }
        }
    }
}

/// Default pipeline over in-memory seams (tests, replay harness).
pub type MemPipeline = Pipeline<
    two_bot_core::MemStore,
    two_bot_core::NoopLeveling,
    two_bot_core::NoopFacts,
    ScriptedInvites,
    NoClassification,
>;

impl MemPipeline {
    /// Build a replay-ready pipeline with scriptable invite counters.
    #[must_use]
    pub fn for_replay() -> Self {
        Pipeline::new(
            two_bot_core::MemStore::new(),
            Some(two_bot_core::NoopLeveling),
            Some(two_bot_core::NoopFacts),
            ScriptedInvites::new(),
            NoClassification,
        )
    }
}

/// Build the shared cache with the S3 resource set (used by the shard runner).
#[must_use]
pub fn build_cache() -> DefaultInMemoryCache {
    DefaultInMemoryCache::builder()
        .resource_types(cache_resource_types())
        .build()
}
