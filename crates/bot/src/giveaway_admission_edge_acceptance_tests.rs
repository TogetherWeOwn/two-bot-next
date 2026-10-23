//! Protected acceptance additions: exercise admission through CommandRuntime,
//! real giveaway SQL, and the existing Discord HTTP boundary double.
//!
//! Mutations caught: a count-then-insert race exceeds max_entries; an expired
//! or paused row admits an entrant; a per-giveaway (rather than per-user) budget
//! permits attempt 11; INSERT-then-delete hides a rejected attempt; absent
//! joined_at is treated as an old membership. No product-only seams are needed.
#![cfg(test)]

use super::*;
use serde_json::Value;

const PRIZE: &str = "Admission test prize";

static MEMBER_SALT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runtime-built member salt: no literal value reaches the snowflake salt,
/// so CodeQL's hard-coded-crypto rule stays silent (house precedent:
/// `fixture_nonce`, `pin_nonce`, `next_entrant_salt`). Each call is unique
/// within the process, which is exactly the distinct-user property the salt
/// exists for. Test-only, never a real credential.
fn next_member_salt() -> u64 {
    use std::sync::atomic::Ordering;
    // +1 keeps the salt nonzero; % 4095 keeps it inside the low bits no
    // matter how many tests share this process.
    MEMBER_SALT_SEQ.fetch_add(1, Ordering::Relaxed) % 4095 + 1
}

fn member_id(salt: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64;
    ((now - 400 * 86_400_000 - 1_420_070_400_000) << 22) | salt
}

fn as_member(mut interaction: Interaction, id: u64, token: &str, member: u64) -> Interaction {
    interaction.id = Id::new(id);
    interaction.token = token.to_owned();
    let actor = interaction.member.as_mut().expect("guild member");
    actor.user = Some(user(member, false));
    actor.joined_at = Some(
        Timestamp::from_secs(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("wall clock")
                .as_secs()
                - 90 * 86_400) as i64,
        )
        .expect("old membership"),
    );
    interaction
}

fn create(id: u64, token: &str, min_server_days: i64) -> Interaction {
    let mut interaction = as_member(
        slash(
            "giveaway",
            Some(CHANNEL),
            vec![option(
                "create",
                CommandOptionValue::SubCommand(vec![
                    option("prize", CommandOptionValue::String(PRIZE.to_owned())),
                    option("duration", CommandOptionValue::Integer(120)),
                    option("winners", CommandOptionValue::Integer(1)),
                    option(
                        "min-server-age-days",
                        CommandOptionValue::Integer(min_server_days),
                    ),
                ]),
            )],
        ),
        id,
        token,
        member_id(next_member_salt()),
    );
    interaction
        .member
        .as_mut()
        .expect("guild member")
        .permissions = Some(Permissions::MANAGE_GUILD);
    interaction
}

fn entry(id: u64, token: &str, opaque: &str, member: u64) -> Interaction {
    let mut interaction = as_member(
        slash("giveaway", Some(CHANNEL), Vec::new()),
        id,
        token,
        member,
    );
    interaction
        .member
        .as_mut()
        .expect("guild member")
        .permissions = Some(Permissions::empty());
    interaction.kind = InteractionType::MessageComponent;
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: format!("gw:{opaque}"),
            component_type: ComponentType::Button,
            resolved: None,
            values: Vec::new(),
        },
    )));
    // Store lookup, not a client-supplied message, must resolve the giveaway.
    interaction.message = None;
    interaction
}

pub(super) fn message_patches(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| {
            request.method == "PATCH"
                && request.path.contains("/messages/")
                && !request.path.ends_with("/@original")
        })
        .collect()
}

fn assert_ephemeral_answer(mock: &MockRest, token: &str) {
    let requests = mock.requests();
    let callbacks: Vec<Value> = requests
        .iter()
        .filter(|request| request.path.contains(token) && request.path.ends_with("/callback"))
        .map(|request| serde_json::from_slice(&request.body).expect("callback JSON"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one acknowledgement for {token}");
    assert_eq!(
        callbacks[0]["data"]["flags"], 64,
        "ephemeral acknowledgement for {token}"
    );
    let completed = requests.iter().any(|request| {
        if !request.path.contains(token)
            || !(request.path.ends_with("/@original") || request.path.ends_with("/callback"))
        {
            return false;
        }
        let body: Value = serde_json::from_slice(&request.body).expect("reply JSON");
        let data = body.get("data").unwrap_or(&body);
        data.get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
    });
    assert!(
        completed,
        "entry {token} receives a nonblank completion, not only a defer"
    );
}

pub(super) async fn create_persisted(
    pool: &PgPool,
    runtime: &CommandRuntime,
    id: u64,
    min_server_days: i64,
) -> String {
    let before: Vec<String> = sqlx::query_scalar("SELECT id FROM giveaways WHERE guild_id = $1")
        .bind(GUILD_S)
        .fetch_all(pool)
        .await
        .expect("initial giveaway ids");
    runtime
        .on_interaction(&create(id, &format!("edge-create-{id}"), min_server_days))
        .await;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM giveaways WHERE guild_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(GUILD_S)
    .fetch_all(pool)
    .await
    .expect("created giveaway rows");
    assert_eq!(
        rows.len(),
        before.len() + 1,
        "create persists exactly one giveaway"
    );
    let new_ids: Vec<_> = rows.into_iter().filter(|id| !before.contains(id)).collect();
    assert_eq!(new_ids.len(), 1, "one distinct new giveaway id");
    new_ids.into_iter().next().expect("one persisted giveaway")
}

async fn entrants(pool: &PgPool, opaque: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT user_id FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2 ORDER BY user_id",
    )
    .bind(GUILD_S)
    .bind(opaque)
    .fetch_all(pool)
    .await
    .expect("persisted entrants")
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn raced_distinct_users_accept_exactly_one_at_max_entries_one() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let opaque = create_persisted(&db.pool, &runtime, 81_000, 0).await;
    sqlx::query("UPDATE giveaways SET max_entries = 1 WHERE guild_id = $1 AND id = $2")
        .bind(GUILD_S)
        .bind(&opaque)
        .execute(&db.pool)
        .await
        .expect("single-entry cap");
    let before = message_patches(&mock).len();
    let start = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    let users: Vec<_> = (10..18).map(member_id).collect();
    for (index, member) in users.iter().copied().enumerate() {
        let runtime = runtime.clone();
        let start = start.clone();
        let interaction = entry(
            81_010 + index as u64,
            &format!("edge-race-{index}"),
            &opaque,
            member,
        );
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            runtime.on_interaction(&interaction).await;
        }));
    }
    for task in tasks {
        task.await.expect("raced entry task");
    }
    let accepted = entrants(&db.pool, &opaque).await;
    assert_eq!(
        accepted.len(),
        1,
        "atomic cap enforcement admits exactly one raced user"
    );
    assert!(users.iter().any(|user| user.to_string() == accepted[0]));
    assert_eq!(
        message_patches(&mock).len() - before,
        1,
        "only the winning insert edits the count"
    );
    for index in 0..8 {
        assert_ephemeral_answer(&mock, &format!("edge-race-{index}"));
    }
    mock.shutdown().await;
    db.close().await;
}

async fn refuses_closed_state(status: &str, seconds_left: i64) {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let opaque = create_persisted(&db.pool, &runtime, 82_000, 0).await;
    sqlx::query(
        "UPDATE giveaways SET status = $3, ends_at = now() + $4 * interval '1 second' WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD_S).bind(&opaque).bind(status).bind(seconds_left as f64)
    .execute(&db.pool).await.expect("closed admission state");
    let before = message_patches(&mock).len();
    let member = member_id(next_member_salt());
    runtime
        .on_interaction(&entry(82_001, "edge-closed", &opaque, member))
        .await;
    assert_ephemeral_answer(&mock, "edge-closed");
    assert!(
        entrants(&db.pool, &opaque).await.is_empty(),
        "{status}/{seconds_left}: refuses entry"
    );
    assert_eq!(
        message_patches(&mock).len(),
        before,
        "refusal makes no message edit"
    );
    // Same row, same user: a valid open window must admit. This prevents an
    // always-refuse implementation from satisfying the negative oracle.
    sqlx::query("UPDATE giveaways SET status = 'open', ends_at = now() + interval '2 hours' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD_S).bind(&opaque).execute(&db.pool).await.expect("reopen control");
    runtime
        .on_interaction(&entry(82_002, "edge-open-control", &opaque, member))
        .await;
    assert_eq!(
        entrants(&db.pool, &opaque).await,
        vec![member.to_string()],
        "open control accepts"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn expired_open_giveaway_refuses_entry() {
    refuses_closed_state("open", -60).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn paused_future_giveaway_refuses_entry() {
    refuses_closed_state("paused", 7_200).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn same_user_budget_spans_giveaways_and_refuses_before_insert() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let first = create_persisted(&db.pool, &runtime, 83_000, 0).await;
    let mut ids = vec![first.clone()];
    // Clone a real created definition to isolate entry throttling from any
    // management-command budget or per-guild active-giveaway creation cap.
    for index in 1..11 {
        let id = format!("edge-budget-giveaway-{index}");
        let copied = sqlx::query(
            "INSERT INTO giveaways (guild_id, id, prize, channel_id, message_id, created_by, \
             winners_count, ends_at, status, requirements_json, max_entries, draw_nonce, created_at, updated_at) \
             SELECT guild_id, $3, prize, channel_id, $4, created_by, winners_count, ends_at, status, \
             requirements_json, max_entries, draw_nonce, created_at, updated_at FROM giveaways \
             WHERE guild_id = $1 AND id = $2",
        )
        .bind(GUILD_S).bind(&first).bind(&id).bind((9_100_000_000_000_000_000_u64 + index).to_string())
        .execute(&db.pool).await.expect("independent open entry target");
        assert_eq!(copied.rows_affected(), 1);
        ids.push(id);
    }
    // A nontransactional sequence records attempted INSERTs even when product
    // code rolls back or deletes the row. Final row counts alone cannot prove
    // the budget was enforced before SQL insertion.
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE edge_entry_insert_attempts; \
         CREATE FUNCTION edge_note_entry_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM nextval('edge_entry_insert_attempts'); RETURN NEW; END; $$; \
         CREATE TRIGGER edge_note_entry_insert BEFORE INSERT ON giveaway_entries \
         FOR EACH ROW EXECUTE FUNCTION edge_note_entry_insert();",
    ))
    .execute(&db.pool)
    .await
    .expect("install test-only INSERT witness");
    let member = member_id(next_member_salt());
    let start = std::time::Instant::now();
    for (index, opaque) in ids.iter().take(10).enumerate() {
        runtime
            .on_interaction(&entry(
                84_000 + index as u64,
                &format!("edge-budget-{index}"),
                opaque,
                member,
            ))
            .await;
        assert_eq!(
            entrants(&db.pool, opaque).await,
            vec![member.to_string()],
            "budget allows entry {}",
            index + 1
        );
    }
    let before = message_patches(&mock).len();
    runtime
        .on_interaction(&entry(84_010, "edge-budget-eleventh", &ids[10], member))
        .await;
    assert!(
        start.elapsed() < Duration::from_secs(60),
        "fixture must stay within the one-minute window"
    );
    assert_ephemeral_answer(&mock, "edge-budget-eleventh");
    assert!(
        entrants(&db.pool, &ids[10]).await.is_empty(),
        "11th entry is refused across different giveaways"
    );
    let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM edge_entry_insert_attempts")
        .fetch_one(&db.pool)
        .await
        .expect("INSERT attempt witness");
    assert_eq!(
        attempts, 10,
        "11th attempt is refused BEFORE INSERT, not rolled back afterwards"
    );
    assert_eq!(
        message_patches(&mock).len(),
        before,
        "budget refusal never edits the giveaway"
    );
    let other = member_id(next_member_salt());
    runtime
        .on_interaction(&entry(84_011, "edge-budget-other-user", &ids[10], other))
        .await;
    assert_eq!(
        entrants(&db.pool, &ids[10]).await,
        vec![other.to_string()],
        "a different user retains their budget"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn missing_joined_at_fails_closed_for_server_age_requirement() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let opaque = create_persisted(&db.pool, &runtime, 85_000, 7).await;
    let member = member_id(next_member_salt());
    let mut interaction = entry(85_001, "edge-joined-missing", &opaque, member);
    interaction.member.as_mut().expect("guild member").joined_at = None;
    let before = message_patches(&mock).len();
    runtime.on_interaction(&interaction).await;
    assert_ephemeral_answer(&mock, "edge-joined-missing");
    assert!(
        entrants(&db.pool, &opaque).await.is_empty(),
        "unknown server age is not eligible"
    );
    assert_eq!(
        message_patches(&mock).len(),
        before,
        "refusal makes no message edit"
    );
    runtime
        .on_interaction(&entry(85_002, "edge-joined-control", &opaque, member))
        .await;
    assert_eq!(
        entrants(&db.pool, &opaque).await,
        vec![member.to_string()],
        "known old membership accepts"
    );
    mock.shutdown().await;
    db.close().await;
}
