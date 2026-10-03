use two_bot_core::member_moderation::{
    DiscordCall, DiscordError, MemberDiscord, MemberModerationService, MemberModerationStore,
    MockMemberDiscord,
};
use two_bot_core::ModerationPolicy;

const GUILD: &str = "100000000000000001";
const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";

#[derive(Clone)]
struct RefusedMember {
    calls: MockMemberDiscord,
    failure: DiscordError,
}

impl MemberDiscord for RefusedMember {
    async fn ban(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.ban(guild, user, reason).await
    }
    async fn kick(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.kick(guild, user, reason).await
    }
    async fn timeout(
        &self,
        guild: &str,
        user: &str,
        until: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        self.calls.timeout(guild, user, until, reason).await
    }
    async fn unban(&self, guild: &str, user: &str, reason: &str) -> Result<(), DiscordError> {
        self.calls.unban(guild, user, reason).await?;
        if user == "refused" {
            Err(self.failure.clone())
        } else {
            Ok(())
        }
    }
}

async fn seed<S: MemberModerationStore>(store: &S, user: &str, due: &str) {
    let attempt = store
        .stage_unban(GUILD, user, due, "expiry", user, NOW)
        .await
        .unwrap();
    store
        .confirm_ban_attempt(GUILD, user, user, attempt, NOW)
        .await
        .unwrap();
    store
        .activate_staged_unban(GUILD, user, user, NOW)
        .await
        .unwrap();
}

fn unbanned_users(calls: &MockMemberDiscord) -> Vec<String> {
    calls
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            DiscordCall::Unban { user_id, .. } => Some(user_id),
            _ => None,
        })
        .collect()
}

// A reconstructed PostgreSQL consumer must see the persisted retry ordering;
// the memory double's factory shares only its backing state. Freeze the clock
// to prove fairness does not depend on wall-clock advance or timer takeover.
pub async fn repeated_refusal_yields<S: MemberModerationStore>(
    make_store: impl Fn() -> S,
    policy: ModerationPolicy,
) {
    let store = make_store();
    seed(&store, "refused", NOW).await;
    seed(&store, "later", DUE).await;
    let calls = MockMemberDiscord::new();
    let discord = RefusedMember {
        calls: calls.clone(),
        failure: DiscordError::Rejected("permission".into()),
    };
    let sweep = || {
        MemberModerationService::new(discord.clone(), make_store(), policy.clone(), || {
            1_700_003_600_000
        })
    };
    assert!(sweep().run_due_unbans(GUILD).await.is_err());
    assert_eq!(unbanned_users(&calls), ["refused"]);
    // A new arrival must not keep pushing this retry behind all fresh work.
    seed(&store, "newer", DUE).await;
    assert!(sweep().run_due_unbans(GUILD).await.is_err());
    assert_eq!(unbanned_users(&calls), ["refused", "later", "refused"]);
    assert!(sweep().run_due_unbans(GUILD).await.is_err());
    assert_eq!(
        unbanned_users(&calls),
        ["refused", "later", "refused", "newer", "refused"]
    );
    assert!(sweep().run_due_unbans(GUILD).await.is_err());
    assert_eq!(calls.call_count("unban"), 6, "one refused attempt per tick");
}

pub async fn unknown_outcome_stays_fenced<S: MemberModerationStore>(
    make_store: impl Fn() -> S,
    policy: ModerationPolicy,
) {
    let store = make_store();
    seed(&store, "refused", NOW).await;
    seed(&store, "later", DUE).await;
    let calls = MockMemberDiscord::new();
    let discord = RefusedMember {
        calls: calls.clone(),
        failure: DiscordError::Timeout,
    };
    let sweep = || {
        MemberModerationService::new(discord.clone(), make_store(), policy.clone(), || {
            1_700_003_600_000
        })
    };
    assert!(sweep().run_due_unbans(GUILD).await.is_err());
    assert_eq!(sweep().run_due_unbans(GUILD).await.unwrap(), 1);
    assert_eq!(sweep().run_due_unbans(GUILD).await.unwrap(), 0);
    assert_eq!(unbanned_users(&calls), ["refused", "later"]);
}
