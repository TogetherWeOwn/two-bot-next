//! Protected behavioral acceptance: rank/leaderboard visibility, mention
//! safety, hostile-name neutralization and fences (R1 preserved contract).
//!
//! These are GREEN controls on the baseline (the current-season board and
//! hostile-name neutralization already shipped): they pin what the R1 builder
//! must NOT change while moving the acknowledgement before the store reads.
//! The failing-first ordering, failure-completion and reply-ownership pins
//! live in `leveling_ack_ordering_acceptance_tests.rs`; the two files are
//! the paired protected suite and neither may be edited by the builder.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on the
//! acceptance card):
//!
//! - Keep the fences: foreign-guild and application-mismatched interactions
//!   stay silent with zero store reads, and rank/leaderboard keep routing
//!   with every feature gate off (`RowGate::Always` — no new gate).
//! - Keep the reply shapes: rank ephemeral, leaderboard public with
//!   `allowedMentions: { parse: [] }` so `<@id>` rows never ping, hostile
//!   display names single-line, mention-neutralized, markdown-escaped and
//!   length-capped, and the defer itself carrying visibility only.
//! - Update the nearby lifecycle contract together with its tests: the
//!   "leveling sends its own immediate callback" note on
//!   `CommandRuntime::on_interaction`, the log-and-abandon leveling branch
//!   below it, and the shared routed test asserting immediate type-4
//!   replies. Record the defer-then-edit lifecycle where that contract
//!   lives; make no new publication or metric claims.
//!
//! Skills used: test-driven-development, verification-before-completion.

use super::*;
use two_bot_core::leveling::{
    leaderboard_reply_for_season, rank_reply, rank_text, RANK_DISPLAY_NAME_LIMIT,
};
use two_bot_core::{HandlerId, SlashContext, SlashOutcome};

/// Read counter for the fence paths: any store read dials this endpoint.
/// (Intentional compact mirror of the ordering file's seam; the two
/// protected files share no helper module so each stands alone.)
struct FakePg {
    port: u16,
    connects: Arc<AtomicU64>,
    handle: JoinHandle<()>,
}

impl FakePg {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake pg listen");
        let port = listener.local_addr().expect("fake pg addr").port();
        let connects = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn({
            let connects = connects.clone();
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    connects.fetch_add(1, Ordering::Relaxed);
                    // Never answer: a fenced path must never get this far,
                    // and an unfenced one fails fast on shutdown.
                    drop(socket);
                }
            }
        });
        Self {
            port,
            connects,
            handle,
        }
    }

    fn pool(&self) -> PgPool {
        PgPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy(&format!(
                "postgres://ack_acceptance@127.0.0.1:{}/ack_acceptance",
                self.port
            ))
            .expect("lazy acceptance pool")
    }

    fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    fn shutdown(&self) {
        self.handle.abort();
    }
}

fn fenced_runtime(pg: &FakePg, origin: String) -> Arc<CommandRuntime> {
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    CommandRuntime::new(
        pg.pool(),
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    )
}

#[tokio::test]
async fn foreign_guild_rank_and_leaderboard_stay_silent_with_zero_reads() {
    let pg = FakePg::start().await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = fenced_runtime(&pg, origin);
    for name in ["rank", "leaderboard"] {
        let mut foreign = slash(name, Some(CHANNEL), Vec::new());
        foreign.guild_id = Some(Id::new(9999));
        runtime.on_interaction(&foreign).await;
    }
    pg.shutdown();

    assert!(
        mock.requests().is_empty(),
        "foreign-guild interactions stay silent"
    );
    assert_eq!(
        pg.connects(),
        0,
        "the guild fence runs before any store read"
    );
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn application_mismatch_stays_silent_with_zero_reads() {
    let pg = FakePg::start().await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = fenced_runtime(&pg, origin);
    // The fixtures carry application 1111; the runtime expects another app.
    runtime.set_identity(999, 2222);
    for name in ["rank", "leaderboard"] {
        runtime
            .on_interaction(&slash(name, Some(CHANNEL), Vec::new()))
            .await;
    }
    pg.shutdown();

    assert!(
        mock.requests().is_empty(),
        "application-mismatched interactions stay silent"
    );
    assert_eq!(
        pg.connects(),
        0,
        "the application fence runs before any store read"
    );
    drop(runtime);
    mock.shutdown().await;
}

#[test]
fn rank_and_leaderboard_route_with_all_feature_gates_off() {
    let router = router_with_commands(gates(false, false));
    for (name, handler) in [
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
    ] {
        let outcome = router.route_slash(&SlashContext {
            name,
            guild_id: Some(GUILD),
            actor_permissions: Some(0),
            custom_row: None,
        });
        assert!(
            matches!(outcome, SlashOutcome::Handled { handler: routed } if routed == handler),
            "{name} stays ungated with every feature off, got {outcome:?}"
        );
    }
}

#[test]
fn rank_reply_shape_stays_ephemeral() {
    let profile = two_bot_core::leveling::LevelProfile {
        guild_id: GUILD_S.to_owned(),
        member_id: "88".to_owned(),
        xp: 1500,
        level: two_bot_core::leveling::level_for_xp(1500),
        message_xp: 1000,
        voice_xp: 500,
        imported_xp: 0,
        rank: Some(1),
        member_count: 1,
        next_level_xp: two_bot_core::leveling::total_xp_for_level(
            two_bot_core::leveling::level_for_xp(1500) + 1,
        ),
    };
    let reply = rank_reply(&profile, "member");
    assert!(reply.ephemeral, "rank stays ephemeral");
    assert_eq!(
        reply.content,
        rank_text("member", profile.level, Some(1), 1, 1500),
        "rank renders the profile it was given"
    );
}

#[test]
fn leaderboard_reply_shape_stays_public_with_suppressed_mentions() {
    let entries = vec![two_bot_core::leveling::LeaderboardEntry {
        member_id: "88".to_owned(),
        xp: 1500,
        level: two_bot_core::leveling::level_for_xp(1500),
        rank: 1,
    }];
    let reply = leaderboard_reply_for_season(&entries, "2026-Q3");
    assert!(
        reply.suppress_mentions,
        "leaderboard keeps mention suppression"
    );
    assert!(
        reply.content.contains("<@88>"),
        "rows keep the non-pinging <@id> shape, got {:?}",
        reply.content
    );
    assert!(
        reply.content.contains("2026-Q3"),
        "seasonal board labels its season, got {:?}",
        reply.content
    );
}

#[test]
fn hostile_rank_name_renders_neutralized_single_line() {
    let rendered = rank_text(
        "<@123> @everyone\n**bold** `code` _x_ [y](z)",
        3,
        Some(1),
        9,
        500,
    );
    assert_eq!(
        rendered.lines().count(),
        3,
        "the hostile name collapses to one line, got {rendered:?}"
    );
    for raw in ["<@", "@everyone", "`code`"] {
        assert!(
            !rendered.contains(raw),
            "no raw {raw:?} survives, got {rendered:?}"
        );
    }
    let name_line = rendered.lines().next().expect("rank has a name line");
    let name_slot = name_line
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
        .expect("the name line keeps the pinned bold wrapper");
    assert!(
        !name_slot.contains("**"),
        "no raw \"**\" survives inside the name slot, got {rendered:?}"
    );
    assert!(
        rendered.contains('\u{200b}'),
        "mentions are neutralized with a zero-width break, got {rendered:?}"
    );
    assert!(
        rendered.contains("\\*\\*bold\\*\\*"),
        "markdown is escaped, got {rendered:?}"
    );
}

#[test]
fn overlong_rank_name_is_capped_within_discord_bounds() {
    let rendered = rank_text(&"x".repeat(3000), 3, Some(1), 9, 500);
    assert!(
        rendered.len() <= 2000,
        "an overlong name cannot overflow the reply, got {} chars",
        rendered.len()
    );
    let plain = rank_text("x", 3, Some(1), 9, 500);
    assert!(
        rendered.len() <= plain.len() + RANK_DISPLAY_NAME_LIMIT,
        "the name contributes at most the documented cap"
    );
}

#[test]
fn deferred_acknowledgement_carries_visibility_only() {
    for (ephemeral, flags) in [(true, 64u64), (false, 0u64)] {
        let defer = two_bot_discord::interactions::deferred_response(ephemeral);
        assert!(
            matches!(
                defer.kind,
                twilight_model::http::interaction::InteractionResponseType::DeferredChannelMessageWithSource
            ),
            "the shared ack is a defer"
        );
        let data = defer.data.expect("defer carries data");
        assert!(
            data.content.is_none(),
            "the defer itself renders no content"
        );
        assert_eq!(
            data.flags.map(|f| f.bits()),
            Some(flags),
            "ephemeral rank defers, public leaderboard defers"
        );
    }
}
