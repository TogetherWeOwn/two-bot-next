//! Protected owner room-access read acceptance. No implementation belongs here.
//!
//! Exercise real slash parsing, authorization, actor dispatch and replies. Only
//! the existing persistence/Discord boundaries are doubles. The worker is seeded
//! before entering the real actor loop so transient requests need no live API.
//! Lists may use optional one-based pages or explicit bounded omission; silent
//! clipping, missing sections, cross-room reads and read-triggered writes fail.
//! Existing durable join-grant hydration is preserved: only absent runtime data
//! without a durable witness is forgotten, not a durable approval.
#![cfg(test)]

use super::*;
use two_bot_core::voice_private::{ChannelId, JoinRequest, MemberId, RequestId};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const OTHER_ROOM: u64 = 501;
const BLOCKED: u64 = 81_000_000_000_000_001;
const GRANTED: u64 = 82_000_000_000_000_002;
const WAITING: u64 = 83_000_000_000_000_003;
const FOREIGN: u64 = 84_000_000_000_000_004;
const COMMAND: &str = "room-access-inspect";
type Worker = GuildRoomWorker<Store, Http>;

async fn worker(private: bool, blocked: &[u64], granted: &[u64], pending: &[u64]) -> Worker {
    let (live, store, http, _) = owned_room();
    let mut guild = command_snapshot();
    guild.channels.extend([
        channel(ROOM, 2, Some(CATEGORY)),
        channel(JOIN, 2, Some(CATEGORY)),
        channel(OTHER_ROOM, 2, Some(CATEGORY)),
    ]);
    guild.members = vec![
        VoiceMember {
            member_id: MEMBER,
            channel_id: ROOM,
            bot: Some(false),
        },
        VoiceMember {
            member_id: 301,
            channel_id: ROOM,
            bot: Some(false),
        },
    ];
    for member in [MEMBER, 301] {
        guild.member_roles.insert(member, vec![]);
    }
    for member in pending {
        guild.members.push(VoiceMember {
            member_id: *member,
            channel_id: JOIN,
            bot: Some(false),
        });
        guild.member_roles.insert(*member, vec![]);
    }
    live.publish(guild);
    store.privacy.lock().unwrap().insert(
        ROOM,
        PrivacyRecord {
            private,
            join_channel_id: private.then_some(JOIN),
            blocked: blocked.iter().copied().collect(),
        },
    );
    if !granted.is_empty() {
        store
            .join_grants
            .lock()
            .unwrap()
            .insert(ROOM, granted.iter().copied().collect());
    }
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .expect("load owner fixture");
    let state = worker
        .privacy
        .get_mut(&ROOM)
        .expect("loaded durable privacy");
    for (index, member) in pending.iter().enumerate() {
        state.pending.insert(
            MemberId(*member),
            JoinRequest {
                id: RequestId(index as u64 + 1),
                member_id: MemberId(*member),
                owner_id: MemberId(MEMBER),
            },
        );
    }
    state.next_request_id = pending.len() as u64 + 1;
    state
        .validate()
        .expect("internally consistent access fixture");
    worker
}

struct ReadFixture {
    runtime: VoiceRuntime<Store, Http>,
    task: tokio::task::JoinHandle<Worker>,
    privacy: HashMap<u64, PrivateRoom>,
    stored_privacy: BTreeMap<u64, PrivacyRecord>,
    grants: BTreeMap<u64, BTreeSet<u64>>,
    rooms: HashMap<u64, VoiceRoom>,
    trace: Trace,
}

impl ReadFixture {
    fn start(mut worker: Worker) -> Self {
        // Setup effects have already settled. Inspect must neither write now
        // nor leave a mutation waiting for a later dispatch.
        let trace = worker.store.trace.clone();
        trace.lock().unwrap().clear();
        let privacy = worker.privacy.clone();
        let stored_privacy = worker.store.privacy.lock().unwrap().clone();
        let grants = worker.store.join_grants.lock().unwrap().clone();
        let rooms = worker.store.rooms.lock().unwrap().clone();
        let runtime = test_runtime(trace.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        runtime.actors.lock().unwrap().insert(
            GUILD,
            GuildActor {
                live: worker.live.clone(),
                tx,
                backpressure: ActorInboxBackpressure::default(),
            },
        );
        let task = tokio::spawn(async move {
            run_actor(&mut worker, rx, Duration::from_secs(3600)).await;
            worker
        });
        Self {
            runtime,
            task,
            privacy,
            stored_privacy,
            grants,
            rooms,
            trace,
        }
    }

    async fn read(
        &self,
        actor: u64,
        permissions: Option<Permissions>,
        options: Vec<CommandDataOption>,
    ) -> String {
        self.invoke(COMMAND, actor, permissions, options).await
    }

    async fn invoke(
        &self,
        command: &str,
        actor: u64,
        permissions: Option<Permissions>,
        options: Vec<CommandDataOption>,
    ) -> String {
        let interaction =
            voice_interaction_as(Some(command_data(command, options)), permissions, actor);
        let responses = Arc::new(Mutex::new(Vec::new()));
        let captured = responses.clone();
        let owned = handle_voice_interaction(&self.runtime, &interaction, move |response| {
            captured.lock().unwrap().push(response);
            async {}
        })
        .await;
        assert!(owned, "/{command} must be handled, not silently dropped");
        let responses = responses.lock().unwrap();
        assert_eq!(responses.len(), 1, "exactly one private read/refusal reply");
        let response = &responses[0];
        let data = response.data.as_ref().expect("reply data");
        assert!(
            data.flags
                .unwrap_or(MessageFlags::empty())
                .contains(MessageFlags::EPHEMERAL),
            "room access must not be public"
        );
        assert!(
            data.components.as_ref().is_none_or(Vec::is_empty),
            "a read must not offer grant/deny/unblock mutations"
        );
        let text = response_text(response);
        assert!(!text.trim().is_empty(), "read/refusal cannot be empty");
        assert!(
            text.encode_utf16().count() <= 2000,
            "plain reply exceeds Discord's content bound"
        );
        text
    }

    async fn unchanged(self) {
        // Closing only the sender avoids GuildDelete changing the live fixture.
        self.runtime.actors.lock().unwrap().remove(&GUILD);
        drop(self.runtime);
        let mut worker = tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("actor stops after inbox closes")
            .expect("actor did not panic");
        assert_eq!(
            worker.privacy, self.privacy,
            "read changed transient access state"
        );
        assert_eq!(
            *worker.store.privacy.lock().unwrap(),
            self.stored_privacy,
            "read persisted privacy changes"
        );
        assert_eq!(
            *worker.store.join_grants.lock().unwrap(),
            self.grants,
            "read changed durable grants"
        );
        assert_eq!(
            *worker.store.rooms.lock().unwrap(),
            self.rooms,
            "read changed ownership/rooms"
        );
        let effects = self.trace.lock().unwrap().clone();
        assert!(
            effects.is_empty(),
            "read caused store/Discord effects: {effects:?}"
        );
        assert!(
            !worker.dispatch_one(u64::MAX).await,
            "read queued a later mutation"
        );
    }
}

fn label(line: &str) -> Option<&'static str> {
    let line = line
        .trim_start_matches([' ', '#', '*', '-', '_'])
        .to_ascii_lowercase();
    if line.starts_with("blocked") {
        Some("blocked")
    } else if line.starts_with("granted") {
        Some("granted")
    } else if line.starts_with("pending") || line.starts_with("waiting") {
        Some("pending")
    } else {
        None
    }
}

fn section(text: &str, wanted: &str) -> String {
    let mut found = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if let Some(name) = label(line) {
            if found {
                break;
            }
            if name == wanted {
                found = true;
                lines.push(line);
            }
        } else if found {
            lines.push(line);
        }
    }
    assert!(found, "missing explicit {wanted} section: {text}");
    lines.join("\n")
}

fn has_id(text: &str, id: u64) -> bool {
    text.split(|ch: char| !ch.is_ascii_digit())
        .any(|token| token == id.to_string())
}

fn listed(text: &str, name: &str, ids: &[u64]) {
    let body = section(text, name);
    for id in ids {
        assert!(has_id(&body, *id), "{name} omitted member {id}: {text}");
    }
}

fn empty(text: &str, name: &str) {
    let body = section(text, name).to_ascii_lowercase();
    assert!(
        [
            "none",
            "empty",
            "no ",
            "(0)",
            ": 0",
            "0 members",
            "0 requests"
        ]
        .iter()
        .any(|word| body.contains(word)),
        "{name} needs plain empty-state copy: {text}"
    );
}

fn page(number: usize) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value: CommandOptionValue::Integer(number as i64),
    }
}

fn page_total(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let tail = lower.split_once("page ")?.1;
    let total = tail.split_once(" of ")?.1;
    total
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[tokio::test]
async fn control_existing_public_dispatch_is_private_and_read_only_when_already_public() {
    let fixture = ReadFixture::start(worker(false, &[], &[], &[]).await);
    let text = fixture.invoke("public", MEMBER, None, vec![]).await;
    assert!(text.contains("already public"), "{text}");
    fixture.unchanged().await;
}

#[tokio::test]
async fn owner_sees_three_distinct_live_sections_without_staff_permissions() {
    let mut worker = worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await;
    let mut other = room(OTHER_ROOM);
    other.owner_id = 301;
    worker.rooms.insert(OTHER_ROOM, other.clone());
    worker.store.rooms.lock().unwrap().insert(OTHER_ROOM, other);
    let foreign_record = PrivacyRecord {
        private: false,
        join_channel_id: None,
        blocked: [FOREIGN].into_iter().collect(),
    };
    worker
        .store
        .privacy
        .lock()
        .unwrap()
        .insert(OTHER_ROOM, foreign_record.clone());
    worker.privacy.insert(
        OTHER_ROOM,
        PrivateRoom::from_record(
            ChannelId(OTHER_ROOM),
            MemberId(301),
            "other",
            &foreign_record,
        )
        .expect("foreign fixture"),
    );
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    listed(&text, "blocked", &[BLOCKED]);
    listed(&text, "granted", &[GRANTED]);
    listed(&text, "pending", &[WAITING]);
    for (name, excluded) in [
        ("blocked", [GRANTED, WAITING]),
        ("granted", [BLOCKED, WAITING]),
        ("pending", [BLOCKED, GRANTED]),
    ] {
        for id in excluded {
            assert!(
                !has_id(&section(&text, name), id),
                "member placed in wrong section: {text}"
            );
        }
    }
    assert!(!has_id(&text, FOREIGN), "another room leaked");
    assert_eq!(
        fixture.read(MEMBER, None, vec![]).await,
        text,
        "a repeated read must not consume requests/grants"
    );
    fixture.unchanged().await;
}

#[tokio::test]
async fn public_transition_keeps_durable_blocks_visible_and_clears_other_sections() {
    let mut worker = worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await;
    let text = worker.apply_privacy(MEMBER, false, "owner", PrivacyCommand::Public);
    assert!(text.contains("Opening"), "public setup failed: {text}");
    for step in 1..=40u64 {
        if !worker.dispatch_one(step * 1_000_000).await {
            break;
        }
        assert!(step < 40, "public setup did not settle");
    }
    assert!(!worker.privacy[&ROOM].private);
    assert!(worker.privacy[&ROOM].blocked.contains(&MemberId(BLOCKED)));
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    listed(&text, "blocked", &[BLOCKED]);
    empty(&text, "granted");
    empty(&text, "pending");
    assert!(!has_id(&text, GRANTED));
    assert!(!has_id(&text, WAITING));
    fixture.unchanged().await;
}

#[tokio::test]
async fn fresh_public_room_has_three_plain_empty_sections() {
    let mut worker = worker(false, &[], &[], &[]).await;
    worker.privacy.remove(&ROOM);
    worker.store.privacy.lock().unwrap().clear();
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    for name in ["blocked", "granted", "pending"] {
        empty(&text, name);
    }
    fixture.unchanged().await;
}

#[tokio::test]
async fn non_owner_gets_existing_owner_refusal_without_any_access_rows() {
    let fixture = ReadFixture::start(worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await);
    let text = fixture.read(301, None, vec![]).await;
    assert!(
        text.contains("Only the room owner or an admin"),
        "existing owner gate refusal: {text}"
    );
    for id in [BLOCKED, GRANTED, WAITING] {
        assert!(!has_id(&text, id), "refusal leaked access data");
    }
    fixture.unchanged().await;
}

#[tokio::test]
async fn existing_admin_exception_is_limited_to_the_callers_current_room() {
    let fixture = ReadFixture::start(worker(true, &[BLOCKED], &[], &[]).await);
    let text = fixture
        .read(301, Some(Permissions::MANAGE_CHANNELS), vec![])
        .await;
    listed(&text, "blocked", &[BLOCKED]);
    fixture.unchanged().await;
}

#[tokio::test]
async fn not_in_voice_does_not_fall_back_to_a_room_the_actor_owns() {
    let worker = worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await;
    worker.live.voice_update_at(MEMBER, None, Some(false), 0);
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    assert!(
        text.contains("You need to be in a voice room"),
        "existing no-room refusal: {text}"
    );
    for id in [BLOCKED, GRANTED, WAITING] {
        assert!(!has_id(&text, id));
    }
    fixture.unchanged().await;
}

#[tokio::test]
async fn untracked_current_channel_cannot_read_a_different_owned_room() {
    let worker = worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await;
    worker
        .live
        .voice_update_at(MEMBER, Some(OTHER_ROOM), Some(false), 0);
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    assert!(
        text.contains("isn't a temporary room I manage"),
        "existing untracked-room refusal: {text}"
    );
    for id in [BLOCKED, GRANTED, WAITING] {
        assert!(!has_id(&text, id));
    }
    fixture.unchanged().await;
}

#[tokio::test]
async fn durable_restart_does_not_reconstruct_grants_or_requests_from_discord_overwrites() {
    let worker = worker(true, &[BLOCKED], &[], &[]).await;
    let mut cached = channel(ROOM, 2, Some(CATEGORY));
    cached.permission_overwrites = Some(vec![PermissionOverwrite {
        allow: Permissions::CONNECT,
        deny: Permissions::empty(),
        id: Id::new(GRANTED),
        kind: PermissionOverwriteType::Member,
    }]);
    worker.live.upsert_channel(cached);
    let fixture = ReadFixture::start(worker);
    let text = fixture.read(MEMBER, None, vec![]).await;
    listed(&text, "blocked", &[BLOCKED]);
    empty(&text, "granted");
    empty(&text, "pending");
    assert!(
        !has_id(&text, GRANTED),
        "a Discord allow is not a current grant witness"
    );
    fixture.unchanged().await;
}

#[tokio::test]
async fn durable_grant_hydration_remains_visible_but_pending_requests_do_not_survive_reload() {
    let before = worker(true, &[BLOCKED], &[GRANTED], &[WAITING]).await;
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    *store.privacy.lock().unwrap() = before.store.privacy.lock().unwrap().clone();
    *store.join_grants.lock().unwrap() = before.store.join_grants.lock().unwrap().clone();
    let reloaded = GuildRoomWorker::load(before.live.clone(), store, Http::new(trace))
        .await
        .expect("restart fixture");
    assert!(
        reloaded.privacy[&ROOM].pending.is_empty(),
        "fixture reload forgets transient requests"
    );
    let fixture = ReadFixture::start(reloaded);
    let text = fixture.read(MEMBER, None, vec![]).await;
    listed(&text, "blocked", &[BLOCKED]);
    listed(&text, "granted", &[GRANTED]);
    empty(&text, "pending");
    assert!(!has_id(&text, WAITING));
    fixture.unchanged().await;
}

#[tokio::test]
async fn long_lists_are_bounded_repeatable_and_never_silently_clip_a_section() {
    let blocked: Vec<_> = (0..100).map(|n| u64::MAX - n).collect();
    let granted: Vec<_> = (100..200).map(|n| u64::MAX - n).collect();
    let pending: Vec<_> = (200..300).map(|n| u64::MAX - n).collect();
    let fixture = ReadFixture::start(worker(true, &blocked, &granted, &pending).await);
    let first = fixture.read(MEMBER, None, vec![]).await;
    assert_eq!(fixture.read(MEMBER, None, vec![]).await, first);
    let total = page_total(&first).unwrap_or(1);
    assert!((1..=1000).contains(&total), "finite, bounded page count");
    let mut pages = vec![first.clone()];
    for number in 2..=total {
        let text = fixture.read(MEMBER, None, vec![page(number)]).await;
        assert_eq!(fixture.read(MEMBER, None, vec![page(number)]).await, text);
        pages.push(text);
    }
    for (name, ids) in [
        ("blocked", &blocked),
        ("granted", &granted),
        ("pending", &pending),
    ] {
        let sections: Vec<_> = pages.iter().map(|text| section(text, name)).collect();
        let shown: Vec<_> = ids
            .iter()
            .filter(|id| sections.iter().any(|body| has_id(body, **id)))
            .collect();
        if total > 1 {
            assert_eq!(
                shown.len(),
                ids.len(),
                "paging must reach every {name} member"
            );
            for id in ids {
                assert_eq!(
                    sections.iter().filter(|body| has_id(body, *id)).count(),
                    1,
                    "paged ID duplicated"
                );
            }
        } else {
            assert!(
                !shown.is_empty(),
                "bounded view still shows some {name} members"
            );
            let body = sections[0].to_ascii_lowercase();
            assert!(
                ["more", "omitt", "truncat", "shown", "showing"]
                    .iter()
                    .any(|word| body.contains(word)),
                "{name} was silently clipped: {first}"
            );
            assert!(
                body.contains("100"),
                "bounded omission must retain the full {name} count"
            );
        }
    }
    for text in &pages {
        let mut tail = text.as_str();
        while let Some((_, rest)) = tail.split_once("<@") {
            let rest = rest.strip_prefix('!').unwrap_or(rest);
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            assert!(
                !digits.is_empty() && rest[digits.len()..].starts_with('>'),
                "member mention clipped: {text}"
            );
            tail = &rest[digits.len()..];
        }
    }
    fixture.unchanged().await;
}
