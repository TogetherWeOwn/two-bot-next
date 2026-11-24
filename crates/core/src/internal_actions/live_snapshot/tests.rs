use std::convert::Infallible;

use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

use super::*;
use crate::{
    clock_guard::ClockGuard,
    internal_actions::{
        burn_in_memory, is_implemented, sign, AuthHeaders, AuthenticatedRequest, KeyRing,
        NonceCache, SigningKey, NONCE_TTL_SECONDS, SKEW_SECONDS,
    },
    realtime_wire::{Guild, Member, Permissions, Role, Topics},
    Secret,
};

const GUILD: Id = Id(NonZeroU64::new(100).unwrap());
const OTHER_GUILD: Id = Id(NonZeroU64::new(101).unwrap());
const BODY: &[u8] = br#"{"action":"live.snapshot","guild_id":"100"}"#;
const READ_KEY: &str = "fixture-read";
const ACTION_KEY: &str = "fixture-actions";
const AUTHORITY: LiveSnapshotReadAuthority<'_> = LiveSnapshotReadAuthority {
    key_id: READ_KEY,
    guild_id: GUILD,
};

fn observed() -> WireTime {
    WireTime(OffsetDateTime::parse("2026-11-21T02:00:00Z", &Rfc3339).unwrap())
}

struct Client {
    keys: KeyRing,
    clock: ClockGuard,
    nonces: NonceCache,
    buckets: TokenBuckets,
    authority: Option<&'static LiveSnapshotReadAuthority<'static>>,
    now: WireTime,
    nonce: u128,
}

impl Client {
    fn new() -> Self {
        Self {
            keys: KeyRing::new(
                [(READ_KEY, [1; 32]), (ACTION_KEY, [2; 32])]
                    .into_iter()
                    .map(|(id, bytes)| SigningKey {
                        id: id.into(),
                        secret: Secret::new(bytes.to_vec()),
                    })
                    .collect(),
            ),
            clock: ClockGuard::new(),
            nonces: NonceCache::new(NONCE_TTL_SECONDS),
            buckets: TokenBuckets::new(),
            authority: Some(&AUTHORITY),
            now: observed(),
            nonce: 1,
        }
    }

    fn request<'a>(
        &mut self,
        raw: &[u8],
        key_id: &str,
        load: impl FnOnce(Id) -> Option<(&'a Recovery, &'a Generation)>,
    ) -> Result<Option<&'a [u8]>, ActionError> {
        // Freeze authentication time independently of the baseline lease tests.
        let seconds = observed().0.unix_timestamp();
        let timestamp = seconds.to_string();
        let nonce = format!("{:032x}", self.nonce);
        self.nonce += 1;
        let signature = sign(
            &[if key_id == READ_KEY { 1 } else { 2 }; 32],
            &timestamp,
            &nonce,
            raw,
        );
        let headers = AuthHeaders {
            key_id,
            timestamp: &timestamp,
            nonce: &nonce,
            signature: &signature,
        };
        let verified = AuthenticatedRequest::verify(
            &headers,
            raw,
            &self.keys,
            SKEW_SECONDS,
            u64::try_from(seconds).unwrap() * 1_000,
            &mut self.clock,
        )?;
        let grant = burn_in_memory(verified, &mut self.nonces)?
            .authorize_live_snapshot(self.authority, &mut self.buckets)?;
        Ok(grant.read(load, || self.now))
    }

    fn refusal(&mut self, raw: &[u8], key: &str) -> ErrorCode {
        self.request(raw, key, |_| {
            panic!("refused request looked up recovery state")
        })
        .unwrap_err()
        .code
    }

    fn snapshot<'a>(
        &mut self,
        source: &'a Recovery,
        generation: &'a Generation,
    ) -> Option<&'a [u8]> {
        self.request(BODY, READ_KEY, |guild| {
            assert!(guild == GUILD);
            Some((source, generation))
        })
        .unwrap()
    }
}

#[test]
fn refuses_missing_authority_wrong_key_wrong_guild_and_malformed_input_before_lookup() {
    let mut client = Client::new();
    client.authority = None;
    assert_eq!(client.refusal(BODY, READ_KEY), ErrorCode::ActionNotAllowed);
    client.authority = Some(&AUTHORITY);
    assert_eq!(
        client.refusal(BODY, ACTION_KEY),
        ErrorCode::ActionNotAllowed
    );
    for raw in [
        br#"{"action":"live.snapshot","guild_id":"101"}"#.as_slice(),
        br#"{"action":"voice.snapshot","guild_id":"100"}"#,
    ] {
        assert_eq!(client.refusal(raw, READ_KEY), ErrorCode::ActionNotAllowed);
    }
    for raw in [
        b"{".as_slice(),
        b"[]",
        br#"{"action":"live.snapshot"}"#,
        br#"{"action":"live.snapshot","guild_id":100}"#,
        br#"{"action":"live.snapshot","guild_id":"0100"}"#,
        br#"{"action":"live.snapshot","guild_id":"+100"}"#,
        br#"{"action":"live.snapshot","guild_id":"0"}"#,
        br#"{"action":"live.snapshot","guild_id":"18446744073709551616"}"#,
        br#"{"action":"live.snapshot","guild_id":"101","guild_\u0069d":"100"}"#,
        br#"{"action":"live.snapshot","guild_id":"100","extra":true}"#,
    ] {
        assert_eq!(client.refusal(raw, READ_KEY), ErrorCode::Malformed);
    }
}

#[test]
fn a_replayed_nonce_cannot_load_state_and_the_verb_stays_unregistered() {
    let mut client = Client::new();
    assert!(!is_implemented("live.snapshot"));
    let raw = b"{";
    assert_eq!(client.refusal(raw, READ_KEY), ErrorCode::Malformed);
    client.nonce -= 1;
    assert_eq!(client.refusal(raw, READ_KEY), ErrorCode::Replayed);
}

fn synthetic_topics(guild_id: Id) -> Topics {
    // The Web schema requires an owner and an everyone role. This invented
    // owner ID carries only the required empty/default fields.
    let owner_id = Id(NonZeroU64::new(300).unwrap());
    Topics {
        guild: Guild {
            owner_id,
            afk_channel_id: None,
        },
        channels: vec![],
        roles: vec![Role {
            id: guild_id,
            permissions: Permissions(0),
        }],
        members: vec![Member {
            user_id: owner_id,
            role_ids: vec![],
            nick: None,
            bot: false,
            pending: false,
        }],
        voice: vec![],
        events: vec![],
    }
}

async fn publish(recovery: &mut Recovery, guild: Id, generation: &Generation, previous: u64) {
    recovery
        .publish(
            generation,
            previous,
            observed(),
            synthetic_topics(guild),
            async |_, _, _| Ok::<_, Infallible>(()),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn returns_exact_web_reply_only_for_a_current_scoped_baseline() {
    let mut client = Client::new();
    let mut recovery = Recovery::new(GUILD);
    let generation = recovery.begin();
    assert_eq!(client.request(BODY, READ_KEY, |_| None).unwrap(), None);
    assert_eq!(client.snapshot(&recovery, &generation), None);

    publish(&mut recovery, GUILD, &generation, 6).await;
    let fixture = include_bytes!("../../../tests/fixtures/live-snapshot.json");
    for age in [-2_000, 0, 29_999] {
        client.now = WireTime(observed().0 + Duration::milliseconds(age));
        assert_eq!(
            client.snapshot(&recovery, &generation),
            Some(fixture.as_slice())
        );
    }
    for age in [-2_001, 30_000] {
        client.now = WireTime(observed().0 + Duration::milliseconds(age));
        assert_eq!(client.snapshot(&recovery, &generation), None);
    }
    client.now = observed();

    let mut other = Recovery::new(OTHER_GUILD);
    let other_generation = other.begin();
    publish(&mut other, OTHER_GUILD, &other_generation, 6).await;
    assert_eq!(client.snapshot(&other, &other_generation), None);

    recovery.retire();
    assert_eq!(client.snapshot(&recovery, &generation), None);
    let next = recovery.begin();
    publish(&mut recovery, GUILD, &next, 8).await;
    assert_eq!(client.snapshot(&recovery, &generation), None);
    let replacement = client.snapshot(&recovery, &next).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(replacement).unwrap()["seq"],
        10
    );
}
