// Protected offline acceptance fixture. No SQL connection, socket, environment
// credential, or production write is available in this executable.
//
// The runtime and permission calculator below are the real, current source.
// Only settings/SQL/REST boundaries are replaced. The renderer wrapper injects
// adversarial text into the real renderer's output so the runtime's substitution
// is tested independently of today's fixed welcome wording.
//
// The store double preserves the current contract: begin locks, failed record
// rolls back, accepted send is external to that transaction. A Builder changing
// that boundary contract must request a correction from the test author rather
// than edit this file. These tests do not establish restart/crash exactly-once.
extern crate two_bot_core as domain;
extern crate two_bot_discord as discord;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use domain::onboarding::{MembershipTrigger, MentionPolicy};
use serde_json::{json, Value};
use twilight_model::channel::message::Component;
use twilight_model::gateway::event::Event;
use tokio::sync::{mpsc, oneshot};

pub use discord::{
    onboarding_config, onboarding_messages, route_interaction, InviteSource, RoutedInteraction,
};
pub use domain::{funnel, ComponentHandler, ComponentOutcome, InteractionRouter, RouterGates};

#[path = "../../../discord/src/onboarding_permissions.rs"]
pub mod onboarding_permissions;
#[path = "../../src/onboarding.rs"]
pub mod subject;

const GUILD: u64 = 22;
const BOT: u64 = 44;
const DESTINATION: u64 = 66;
const MEMBER: u64 = 77;
const NOW_MS: i64 = 1_800_000_000_000;
const TRAPS: &str = "\nfixture: bare=1175127344072118405 prefix=x1175127344072118405 longer=11751273440721184050 user=<@1175127344072118405> role=<@&1175127344072118405> other=<#11751273440721184050>";
static INJECT_TRAPS: OnceLock<bool> = OnceLock::new();

pub mod onboarding {
    pub use domain::onboarding::*;

    pub fn adjudicate_welcome(
        mode: OnboardingMode,
        member_id: u64,
        landing: Option<&str>,
        anchor: &str,
        now: i64,
        spec: AnchorSpec,
        dry_run: bool,
    ) -> WelcomeEffect {
        let mut effect = domain::onboarding::adjudicate_welcome(
            mode, member_id, landing, anchor, now, spec, dry_run,
        );
        if *super::INJECT_TRAPS.get().expect("fixture configured") {
            if let WelcomeEffect::Post { content, .. } = &mut effect {
                content.push_str(super::TRAPS);
            }
        }
        effect
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Error;

#[derive(Default)]
pub struct StoreState {
    prompted: HashSet<(String, String)>,
    claims: HashSet<(String, String)>,
    fail_record: usize,
    pub record_attempts: usize,
}

#[derive(Clone, Default)]
pub struct PgPool(Arc<Mutex<StoreState>>);

pub struct Transaction(());

impl PgPool {
    pub async fn begin(&self) -> Result<Transaction, Error> {
        panic!("picker SQL is outside this welcome fixture")
    }
}

impl std::ops::Deref for Transaction {
    type Target = ();
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Transaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Transaction {
    pub async fn commit(self) -> Result<(), Error> {
        panic!("picker SQL is outside this welcome fixture")
    }
}

pub struct Query;
pub fn query(_: &str) -> Query {
    Query
}
impl Query {
    pub fn bind<T>(self, _: T) -> Self {
        self
    }
    pub async fn execute<T>(self, _: T) -> Result<(), Error> {
        panic!("picker SQL is outside this welcome fixture")
    }
}

pub mod settings {
    pub use domain::settings::*;
    pub struct SettingsStore;
    impl SettingsStore {
        pub fn new(_: &super::PgPool) -> Self {
            Self
        }
        pub async fn load_snapshot(&self) -> Result<SettingsSnapshot, super::Error> {
            Ok(SettingsSnapshot::default())
        }
    }
}

pub mod onboarding_store {
    use super::*;
    use domain::onboarding::{GameSelection, SessionPlan};

    pub async fn has_onboarding_prompt(
        pool: &PgPool,
        guild: &str,
        member: &str,
    ) -> Result<bool, Error> {
        Ok(pool
            .0
            .lock()
            .expect("fixture store")
            .prompted
            .contains(&(guild.into(), member.into())))
    }

    pub struct PromptGuard {
        pool: PgPool,
        key: (String, String),
    }

    pub async fn begin_prompt(
        pool: &PgPool,
        guild: &str,
        member: &str,
    ) -> Result<Option<PromptGuard>, Error> {
        let key = (guild.into(), member.into());
        let mut state = pool.0.lock().expect("fixture store");
        if state.prompted.contains(&key) {
            return Ok(None);
        }
        assert!(
            state.claims.insert(key.clone()),
            "unexpected concurrent claim in sequential fixture"
        );
        Ok(Some(PromptGuard {
            pool: pool.clone(),
            key,
        }))
    }

    impl PromptGuard {
        async fn record(self) -> Result<bool, Error> {
            let mut state = self.pool.0.lock().expect("fixture store");
            state.record_attempts += 1;
            if state.fail_record > 0 {
                state.fail_record -= 1;
                return Err(Error);
            }
            Ok(state.prompted.insert(self.key.clone()))
        }
        pub async fn record_sent(self, _: &str, _: &str) -> Result<bool, Error> {
            self.record().await
        }
        pub async fn record_anchor_sent(self, _: &str, _: &str) -> Result<bool, Error> {
            self.record().await
        }
    }
    impl Drop for PromptGuard {
        fn drop(&mut self) {
            self.pool
                .0
                .lock()
                .expect("fixture store")
                .claims
                .remove(&self.key);
        }
    }

    pub async fn record_game_selection_in_transaction(
        _: &mut Transaction,
        _: &str,
        _: &str,
        _: &[String],
        _: &GameSelection,
        _: &str,
    ) -> Result<(), Error> {
        panic!("picker record is outside this fixture")
    }

    pub async fn record_session_routed_in_transaction(
        _: &mut Transaction,
        _: &str,
        _: &str,
        _: &SessionPlan,
        _: &str,
    ) -> Result<bool, Error> {
        panic!("picker record is outside this fixture")
    }
}

pub mod gateway {
    use std::marker::PhantomData;
    use twilight_cache_inmemory::InMemoryCache;
    pub struct GatewayPipeline<I> {
        pub cache: InMemoryCache,
        pub invites: PhantomData<I>,
    }
    impl<I> GatewayPipeline<I> {
        pub fn cache(&self) -> &InMemoryCache {
            &self.cache
        }
    }
}

#[derive(Clone, Debug)]
pub struct VisiblePost {
    pub channel: String,
    pub content: String,
    pub mentions: MentionPolicy,
    pub components: usize,
}

// Socket-free mock REST boundary: freeze a response, signal its arrival, then
// return it only after the test captures an invalidating gateway event. No timer
// or production hook chooses the interleaving, and the stale response stays stale.
struct MockRestReadGate {
    path: &'static str,
    remaining: usize,
    arrived: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

pub struct RestState {
    read_gate: Option<MockRestReadGate>,
    pub reads: Vec<String>,
    pub posts: Vec<VisiblePost>,
    pub fail_send: usize,
    pub fail_identity: usize,
    pub permissions: u64,
    pub channel_denied: bool,
    pub deny_member: bool,
    pub send_attempts: usize,
}
impl Default for RestState {
    fn default() -> Self {
        Self {
            read_gate: None,
            reads: vec![],
            posts: vec![],
            fail_send: 0,
            fail_identity: 0,
            permissions: 3072,
            channel_denied: false,
            deny_member: false,
            send_attempts: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct ActionExecutor(pub Arc<Mutex<RestState>>);
impl ActionExecutor {
    pub async fn get_json_checked(&self, path: &str) -> Result<Option<Value>, Error> {
        let response = self.response(path);
        let arrived = {
            let mut state = self.0.lock().expect("fixture REST");
            state.read_gate.as_mut().and_then(|gate| {
                if gate.path == path && gate.remaining > 0 {
                    gate.remaining -= 1;
                    Some(gate.arrived.clone())
                } else {
                    None
                }
            })
        };
        if let Some(arrived) = arrived {
            let (release, released) = oneshot::channel();
            arrived.send(release).expect("race driver receives REST arrival");
            released.await.expect("race driver releases frozen response");
        }
        response
    }
    fn response(&self, path: &str) -> Result<Option<Value>, Error> {
        let mut state = self.0.lock().expect("fixture REST");
        state.reads.push(path.into());
        if path == "/users/@me" {
            return Ok(Some(json!({"id":"44","bot":true})));
        }
        if path == "/guilds/22" {
            if state.fail_identity > 0 {
                state.fail_identity -= 1;
                return Err(Error);
            }
            return Ok(Some(json!({"id":"22","owner_id":"99"})));
        }
        if path == "/guilds/22/roles" {
            return Ok(Some(
                json!([{"id":"22","permissions":state.permissions.to_string()}]),
            ));
        }
        if path == "/guilds/22/members/44" {
            if state.deny_member {
                return Ok(None);
            }
            return Ok(Some(json!({"user":{"id":"44"},"roles":[]})));
        }
        if let Some(member) = path.strip_prefix("/guilds/22/members/") {
            let Ok(member) = member.parse::<u64>() else {
                return Ok(None);
            };
            if !(MEMBER..MEMBER + 8).contains(&member) {
                return Ok(None);
            }
            return Ok(Some(json!({
                "user":{"id":member.to_string(),"bot":false},
                "roles":[],
                "pending":false
            })));
        }
        if path == "/channels/66" {
            let deny = if state.channel_denied { "2048" } else { "0" };
            return Ok(Some(json!({"id":"66","guild_id":"22","type":0,
                "permission_overwrites":[{"id":"22","type":0,"allow":"0","deny":deny}]})));
        }
        panic!("unexpected fixture REST read: {path}")
    }
    pub async fn get_json(&self, path: &str) -> Result<Option<Value>, Error> {
        self.get_json_checked(path).await
    }
    // Preserve the strict GET boundary's present, absent, and failed outcomes.
    pub async fn get_json_strict(&self, path: &str) -> Result<Option<Value>, Error> {
        self.get_json_checked(path).await
    }
    pub async fn post_channel_message(
        &self,
        channel: &str,
        content: &str,
        components: &[Component],
        mentions: MentionPolicy,
    ) -> Result<String, Error> {
        let mut state = self.0.lock().expect("fixture REST");
        state.send_attempts += 1;
        if state.fail_send > 0 {
            state.fail_send -= 1;
            return Err(Error);
        }
        state.posts.push(VisiblePost {
            channel: channel.into(),
            content: content.into(),
            mentions,
            components: components.len(),
        });
        Ok((9000 + state.posts.len()).to_string())
    }
    pub async fn answer_interaction<T>(&self, _: u64, _: &str, _: &T) -> Result<(), Error> {
        panic!("interaction callback is outside this fixture")
    }
    pub async fn set_member_role(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: bool,
        _: &str,
    ) -> Result<(), Error> {
        panic!("role mutation is outside this fixture")
    }
    pub async fn edit_interaction_response_with_blocked_retry(
        &self,
        _: u64,
        _: &str,
        _: &str,
    ) -> Result<(), Error> {
        panic!("interaction edit is outside this fixture")
    }
}

struct Fixture {
    runtime: subject::OnboardingRuntime,
    pool: PgPool,
    rest: ActionExecutor,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let pool = PgPool::default();
        let rest = ActionExecutor::default();
        let vars = HashMap::from([
            ("TWO_ONBOARDING_MODE".into(), mode.into()),
            ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
            (
                "DISCORD_LANDING_CHANNEL_IDS".into(),
                DESTINATION.to_string(),
            ),
            (
                "DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(),
                DESTINATION.to_string(),
            ),
        ]);
        let runtime =
            subject::OnboardingRuntime::new(pool.clone(), rest.clone(), &vars, GUILD, BOT)
                .expect("valid offline fixture configuration");
        Self {
            runtime,
            pool,
            rest,
        }
    }
    async fn welcome(&self, member: u64, now: i64) -> Result<(), subject::RuntimeError> {
        self.runtime
            .handle(
                subject::OnboardingJob::Welcome {
                    guild_id: GUILD,
                    member_id: member,
                    bot: false,
                    pending: false,
                    trigger: MembershipTrigger::Joined { pending: false },
                    roles: vec![],
                },
                now,
            )
            .await
    }
    fn bot_access_reads(&self) -> usize {
        self.rest
            .0
            .lock()
            .expect("fixture REST")
            .reads
            .iter()
            .filter(|path| {
                matches!(
                    path.as_str(),
                    "/guilds/22" | "/guilds/22/roles" | "/guilds/22/members/44"
                )
            })
            .count()
    }
    fn recipient_reads(&self) -> Vec<u64> {
        self.rest
            .0
            .lock()
            .expect("fixture REST")
            .reads
            .iter()
            .filter_map(|path| {
                path.strip_prefix("/guilds/22/members/")
                    .and_then(|member| member.parse::<u64>().ok())
            })
            .filter(|member| *member != BOT)
            .collect()
    }
    fn visible(&self) -> usize {
        self.rest.0.lock().expect("fixture REST").posts.len()
    }
    fn prompted(&self, member: u64) -> bool {
        self.pool
            .0
            .lock()
            .expect("fixture store")
            .prompted
            .contains(&(GUILD.to_string(), member.to_string()))
    }
}

async fn anchor_traps() {
    let fixture = Fixture::new("anchor");
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("welcome succeeds");
    let state = fixture.rest.0.lock().expect("fixture REST");
    assert_eq!(state.posts.len(), 1);
    let content = &state.posts[0].content;
    assert!(
        content.contains("<#66>"),
        "the delimited anchor placeholder must resolve to the configured channel: {content}"
    );
    assert!(content.ends_with(TRAPS), "substitution must leave bare ids, longer ids, user/role mentions and unrelated channel mentions unchanged: {content}");
}

async fn repeated_window() {
    let fixture = Fixture::new("legacy");
    for member in MEMBER..MEMBER + 8 {
        fixture
            .welcome(member, NOW_MS)
            .await
            .expect("welcome succeeds");
    }
    assert_eq!(
        fixture.visible(),
        8,
        "distinct members must all be welcomed"
    );
    assert_eq!(
        fixture.bot_access_reads(),
        3,
        "guild, roles and bot-member identity load once per TTL window, not once per welcome"
    );
    assert_eq!(
        fixture.recipient_reads(),
        (MEMBER..MEMBER + 8).collect::<Vec<_>>(),
        "each welcome reads its own current recipient document"
    );
}

async fn expiry() {
    let fixture = Fixture::new("legacy");
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("first welcome");
    let cold = fixture.bot_access_reads();
    fixture
        .welcome(MEMBER + 1, NOW_MS)
        .await
        .expect("warm welcome");
    let warm = fixture.bot_access_reads();
    // No mandated TTL length: this is only a conservative finite-expiry bound.
    // Advance both clocks, accepting either event-clock or monotonic expiry.
    tokio::time::advance(std::time::Duration::from_secs(86_400)).await;
    fixture.rest.0.lock().expect("fixture REST").permissions = 1024;
    fixture
        .welcome(MEMBER + 2, NOW_MS + 86_400_000)
        .await
        .expect("expired access is denied, not an error");
    let expired = fixture.bot_access_reads();
    assert_eq!(
        fixture.visible(),
        2,
        "expired identity evidence must not retain revoked Send Messages"
    );
    assert_eq!(
        (cold, warm, expired),
        (3, 3, 6),
        "warm cache is reused, expiry reloads all identity evidence"
    );
}

async fn invalidation() {
    let fixture = Fixture::new("legacy");
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("first welcome");
    fixture
        .welcome(MEMBER + 1, NOW_MS)
        .await
        .expect("warm welcome");
    let warm = fixture.bot_access_reads();
    fixture.rest.0.lock().expect("fixture REST").permissions = 1024;
    let update: twilight_model::gateway::payload::incoming::RoleUpdate =
        serde_json::from_value(json!({
            "guild_id":"22", "role":{"id":"22","name":"@everyone","permissions":"1024",
                "position":0,"color":0,"colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null},
                "flags":0,"hoist":false,"managed":false,"mentionable":false,
                "icon":null,"tags":null,"unicode_emoji":null}
        }))
        .expect("role update fixture");
    // Capture needs only the pipeline cache; the invite source is never called.
    let pipeline = gateway::GatewayPipeline::<discord::NoInvites> {
        cache: twilight_cache_inmemory::InMemoryCache::new(),
        invites: std::marker::PhantomData,
    };
    invalidate_capture(&fixture.runtime, Event::RoleUpdate(update), &pipeline);
    fixture
        .welcome(MEMBER + 2, NOW_MS)
        .await
        .expect("post-invalidation denial");
    assert_eq!(
        fixture.visible(),
        2,
        "a role update invalidates the bot access snapshot before the next welcome"
    );
    assert_eq!(
        (warm, fixture.bot_access_reads()),
        (3, 6),
        "invalidation keeps the prior warm hit and then reloads once"
    );
}

fn invalidate_capture<I>(
    runtime: &subject::OnboardingRuntime,
    event: Event,
    pipeline: &gateway::GatewayPipeline<I>,
) where
    I: InviteSource,
{
    let _ = runtime.capture(&event, pipeline);
}

async fn record_retry(mode: &str) {
    let fixture = Fixture::new(mode);
    fixture.pool.0.lock().expect("fixture store").fail_record = 1;
    let _ = fixture.welcome(MEMBER, NOW_MS).await;
    assert!(
        fixture
            .pool
            .0
            .lock()
            .expect("fixture store")
            .record_attempts
            > 0,
        "record-failure injection must have been reached"
    );
    assert_eq!(
        fixture.visible(),
        1,
        "Discord accepted the first welcome before injected record failure"
    );
    fixture
        .welcome(MEMBER, NOW_MS + 1)
        .await
        .expect("retry completes");
    assert_eq!(
        fixture.visible(),
        1,
        "record failure must not cause a second visible welcome on retry"
    );
    assert!(
        fixture.prompted(MEMBER),
        "accepted welcome eventually has its prompt marker"
    );
}

async fn failed_send(mode: &str) {
    let fixture = Fixture::new(mode);
    fixture.rest.0.lock().expect("fixture REST").fail_send = 1;
    assert!(
        fixture.welcome(MEMBER, NOW_MS).await.is_err(),
        "injected send is refused"
    );
    assert_eq!(fixture.visible(), 0);
    assert!(
        !fixture.prompted(MEMBER),
        "failed send cannot consume the prompt marker"
    );
    fixture
        .welcome(MEMBER, NOW_MS + 1)
        .await
        .expect("failed send may retry");
    assert_eq!(fixture.visible(), 1);
    assert!(fixture.prompted(MEMBER));
}

async fn duplicate_control() {
    let fixture = Fixture::new("anchor");
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("first welcome");
    fixture
        .welcome(MEMBER, NOW_MS + 1)
        .await
        .expect("duplicate event");
    assert_eq!(fixture.visible(), 1);
    let state = fixture.rest.0.lock().expect("fixture REST");
    assert_eq!(state.posts[0].channel, "66");
    assert_eq!(state.posts[0].mentions, MentionPolicy::Member(MEMBER));
    assert_eq!(state.posts[0].components, 0, "anchor welcome has no picker");
}

async fn live_channel_denial() {
    let fixture = Fixture::new("legacy");
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("first welcome");
    fixture.rest.0.lock().expect("fixture REST").channel_denied = true;
    fixture
        .welcome(MEMBER + 1, NOW_MS)
        .await
        .expect("channel denial is a skip");
    assert_eq!(
        fixture.visible(),
        1,
        "identity caching must not cache a channel permission grant"
    );
    assert!(!fixture.prompted(MEMBER + 1));
}

async fn unavailable_identity() {
    let fixture = Fixture::new("legacy");
    fixture.rest.0.lock().expect("fixture REST").fail_identity = 1;
    assert!(fixture.welcome(MEMBER, NOW_MS).await.is_err());
    assert_eq!(fixture.visible(), 0);
    assert!(!fixture.prompted(MEMBER));
    fixture
        .welcome(MEMBER, NOW_MS + 1)
        .await
        .expect("unavailable identity is not a cached denial");
    assert_eq!(fixture.visible(), 1);
}

async fn missing_identity() {
    let fixture = Fixture::new("legacy");
    fixture.rest.0.lock().expect("fixture REST").deny_member = true;
    fixture
        .welcome(MEMBER, NOW_MS)
        .await
        .expect("missing bot is a skip");
    assert_eq!(fixture.visible(), 0);
    assert!(!fixture.prompted(MEMBER));
    fixture.rest.0.lock().expect("fixture REST").deny_member = false;
    fixture
        .welcome(MEMBER, NOW_MS + 1)
        .await
        .expect("absence is not cached as a denial");
    assert_eq!(fixture.visible(), 1);
}

#[derive(Clone, Copy)]
enum AccessInvalidation {
    Role,
    Guild,
    BotMember,
}

impl AccessInvalidation {
    fn event(self) -> Event {
        match self {
            Self::Role => Event::RoleUpdate(serde_json::from_value(json!({
                "guild_id":"22", "role":{"id":"22","name":"@everyone","permissions":"3072",
                    "position":0,"color":0,"colors":{"primary_color":0,"secondary_color":null,"tertiary_color":null},
                    "flags":0,"hoist":false,"managed":false,"mentionable":false,
                    "icon":null,"tags":null,"unicode_emoji":null}
            })).expect("role update fixture")),
            Self::Guild => {
                let guild = serde_json::from_value(json!({
                    "id":"22", "name":"fixture guild", "owner_id":"99",
                    "afk_timeout":300, "default_message_notifications":0,
                    "explicit_content_filter":0, "features":[], "emojis":[],
                    "large":false, "mfa_level":0, "nsfw_level":0,
                    "preferred_locale":"en-US", "premium_progress_bar_enabled":false,
                    "system_channel_flags":0, "verification_level":0,
                    "stage_instances":[], "stickers":[], "voice_states":[],
                    "channels":[], "roles":[], "members":[]
                })).expect("guild create fixture");
                Event::GuildCreate(Box::new(
                    twilight_model::gateway::payload::incoming::GuildCreate::Available(guild),
                ))
            }
            Self::BotMember => Event::MemberUpdate(Box::new(serde_json::from_value(json!({
                "guild_id":"22", "user":{"id":"44","username":"fixture-bot","discriminator":"0","bot":true},
                "roles":[], "pending":false, "joined_at":"2026-09-28T00:00:00.000000+00:00"
            })).expect("bot member update fixture"))),
        }
    }
}

// Catch fatal-on-first-invalidation, stale grant reuse, and unbounded/fourth
// attempts. The real runtime and real MemberAccess own every permission decision.
async fn invalidation_during_read(
    path: &'static str,
    event: AccessInvalidation,
    revoke: bool,
    exhaust: bool,
) {
    let fixture = Fixture::new("legacy");
    let (arrived, mut arrivals) = mpsc::unbounded_channel();
    fixture.rest.0.lock().expect("fixture REST").read_gate = Some(MockRestReadGate {
        path,
        remaining: if exhaust { 3 } else { 1 },
        arrived,
    });
    let pipeline = gateway::GatewayPipeline::<discord::NoInvites> {
        cache: twilight_cache_inmemory::InMemoryCache::new(),
        invites: std::marker::PhantomData,
    };
    let welcome = fixture.welcome(MEMBER, NOW_MS);
    tokio::pin!(welcome);
    let mut invalidations = 0;
    let outcome = loop {
        tokio::select! {
            result = &mut welcome => break result,
            release = arrivals.recv() => {
                let release = release.expect("mock REST arrival channel remains open");
                assert_eq!(fixture.visible(), 0, "no post before access evidence settles");
                assert!(!fixture.prompted(MEMBER));
                assert_eq!(fixture.pool.0.lock().expect("fixture store").record_attempts, 0);
                // The response was already frozen with a grant. Only later
                // reads see the revocation; reusing that old response is unsafe.
                if revoke {
                    let mut state = fixture.rest.0.lock().expect("fixture REST");
                    if path == "/guilds/22/roles" {
                        state.permissions = 1024;
                    } else {
                        state.channel_denied = true;
                    }
                }
                invalidate_capture(&fixture.runtime, event.event(), &pipeline);
                invalidations += 1;
                release.send(()).expect("runtime still awaits the frozen REST response");
            }
        }
    };
    // Print the early outcome before assertions so a fatal first invalidation
    // is distinguishable from a permission denial or a broken fixture in RED logs.
    std::io::Write::write_all(
        &mut std::io::stdout(),
        format!("race path={path} invalidations={invalidations} outcome={outcome:?}\n").as_bytes(),
    )
    .expect("race diagnostic output");
    assert!(
        fixture
            .pool
            .0
            .lock()
            .expect("fixture store")
            .claims
            .is_empty(),
        "all completed attempts release their prompt claim"
    );
    if exhaust {
        assert!(
            matches!(outcome, Err(subject::RuntimeError::Discord)),
            "sustained invalidation must return retryable unavailable evidence"
        );
        assert_eq!(
            fixture.visible(),
            0,
            "exhaustion cannot use the last stale grant"
        );
        assert!(!fixture.prompted(MEMBER));
        assert_eq!(
            fixture.rest.0.lock().expect("fixture REST").send_attempts,
            0
        );
        assert_eq!(
            fixture
                .pool
                .0
                .lock()
                .expect("fixture store")
                .record_attempts,
            0
        );
        assert_eq!(
            invalidations, 3,
            "three attempts, not a fatal first invalidation"
        );
        assert_eq!(
            fixture.bot_access_reads(),
            9,
            "each attempt reloads identity evidence"
        );
        assert_eq!(fixture.rest.0.lock().expect("fixture REST").reads.iter()
            .filter(|read| read.as_str() == "/channels/66").count(),
            if path == "/channels/66" { 3 } else { 0 },
            "exhausted loads never reach channel checks; exhausted channel checks never get a fourth try");
        // The gate allows a fourth response, but the first call must have
        // stopped without a post. A NEW job gets its own finite retry budget.
        fixture
            .welcome(MEMBER, NOW_MS + 1)
            .await
            .expect("later stable job recovers");
        assert_eq!(fixture.visible(), 1);
        assert!(fixture.prompted(MEMBER));
        assert_eq!(
            fixture.bot_access_reads(),
            12,
            "exhausted evidence was not published as current"
        );
    } else {
        outcome.expect("one transient invalidation must reload, not fail the worker");
        assert_eq!(invalidations, 1, "the requested in-flight race was reached");
        assert_eq!(
            fixture.bot_access_reads(),
            6,
            "discard the invalidated snapshot and reload all identity evidence"
        );
        assert_eq!(
            fixture
                .rest
                .0
                .lock()
                .expect("fixture REST")
                .reads
                .iter()
                .filter(|read| read.as_str() == "/channels/66")
                .count(),
            if path == "/channels/66" { 2 } else { 1 },
            "channel evidence is checked afresh after a raced channel response"
        );
        assert_eq!(
            fixture.visible(),
            usize::from(!revoke),
            "only fresh evidence can authorize a post"
        );
        assert_eq!(fixture.prompted(MEMBER), !revoke);
        assert_eq!(
            fixture.rest.0.lock().expect("fixture REST").send_attempts,
            usize::from(!revoke)
        );
        assert_eq!(
            fixture
                .pool
                .0
                .lock()
                .expect("fixture store")
                .record_attempts,
            usize::from(!revoke)
        );
        if !revoke {
            fixture
                .welcome(MEMBER, NOW_MS + 1)
                .await
                .expect("redelivery dedupes");
            fixture
                .welcome(MEMBER + 1, NOW_MS + 2)
                .await
                .expect("fresh snapshot remains warm");
            assert_eq!(
                fixture.visible(),
                2,
                "one post per distinct member after recovery"
            );
            assert!(fixture.prompted(MEMBER + 1));
            assert_eq!(
                fixture.bot_access_reads(),
                6,
                "successful reloaded evidence is cached"
            );
        }
    }
}

fn main() {
    let case = std::env::args().nth(1).expect("acceptance case name");
    INJECT_TRAPS
        .set(case == "anchor_traps")
        .expect("configure once");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("offline Tokio runtime");
    runtime.block_on(async {
        tokio::time::pause();
        match case.as_str() {
            "anchor_traps" => anchor_traps().await,
            "repeated_window" => repeated_window().await,
            "expiry" => expiry().await,
            "invalidation" => invalidation().await,
            "record_retry_legacy" => record_retry("legacy").await,
            "record_retry_anchor" => record_retry("anchor").await,
            "failed_send" => failed_send("legacy").await,
            "failed_send_anchor" => failed_send("anchor").await,
            "missing_identity" => missing_identity().await,
            "duplicate_control" => duplicate_control().await,
            "live_channel_denial" => live_channel_denial().await,
            "unavailable_identity" => unavailable_identity().await,
            "roles_invalidation" => {
                invalidation_during_read("/guilds/22/roles", AccessInvalidation::Role, false, false)
                    .await
            }
            "channel_invalidation" => {
                invalidation_during_read("/channels/66", AccessInvalidation::Role, false, false)
                    .await
            }
            "guild_create_invalidation" => {
                invalidation_during_read(
                    "/guilds/22/roles",
                    AccessInvalidation::Guild,
                    false,
                    false,
                )
                .await
            }
            "bot_member_invalidation" => {
                invalidation_during_read(
                    "/channels/66",
                    AccessInvalidation::BotMember,
                    false,
                    false,
                )
                .await
            }
            "roles_invalidation_revoked" => {
                invalidation_during_read("/guilds/22/roles", AccessInvalidation::Role, true, false)
                    .await
            }
            "channel_invalidation_revoked" => {
                invalidation_during_read("/channels/66", AccessInvalidation::Role, true, false)
                    .await
            }
            "roles_invalidation_exhausted" => {
                invalidation_during_read("/guilds/22/roles", AccessInvalidation::Role, false, true)
                    .await
            }
            "channel_invalidation_exhausted" => {
                invalidation_during_read("/channels/66", AccessInvalidation::Role, false, true)
                    .await
            }
            _ => panic!("unknown acceptance case: {case}"),
        }
    });
    std::io::Write::write_all(&mut std::io::stdout(), format!("PASS {case}\n").as_bytes())
        .expect("acceptance result output");
}
