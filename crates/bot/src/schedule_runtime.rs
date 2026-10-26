//! Scheduled-message commands (`/schedule`, `/schedule-remove`,
//! `/schedule-list`; parity §1 rows 17–19, slice of TOG-9809).
//!
//! Thin wiring over the `scheduled` domain + `scheduled_store` (TOG-10081):
//! decode options → validate → guild-scoped CRUD → `automation_audit_log`
//! row → ephemeral completion. Routing, the `TWO_AUTOMATIONS` gate and the
//! `ManageGuild` refusal all happen in the shared router before these run,
//! so the handlers assume an authorized automations invocation. A schedule
//! binds to the invoking channel, like `/feed-add` binds its relay.
//!
//! Audit parity with legacy `AutomationService`: creates and replaces audit
//! `scheduled.create` / `scheduled.update` (`ok`, `rejected` on validation
//! failure), removes audit `scheduled.delete` (`ok` / `absent`), and
//! successful lists write no row. The seven store-failure refusals (create
//! read and write, remove resolve and delete, list first read, and the two
//! deep-pagination list reads: hidden-window and unknown-overflow) each
//! audit one row in the same refusal shape (`scheduled.create` /
//! `scheduled.delete` / `scheduled.list`, outcome `rejected`, the invoking
//! member as actor, the log line's fixed error-class token as reason, the
//! attempted id or prefix as target, none for lists); a dead pool degrades
//! to warn-plus-safe-reply because the audit write itself fails. The 15 s
//! ticker is a sibling card's job — nothing here posts to channels.

use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction, InteractionData,
};
use two_bot_core::{
    automation_quota::QuotaWriteError,
    commands::MAX_RESOURCE_ID_CHARS,
    funnel::now_millis_for_test,
    message_safety::{text_len, truncate},
    scheduled::{
        format_iso_ms, next_run_at_ms, no_such_schedule_text, no_unique_match_text,
        schedule_confirm_text, schedule_list_channel_pages, schedule_list_first_pages,
        schedule_list_line, schedule_list_overflow_for_count, schedule_list_overflow_line,
        schedule_list_page, schedule_list_page_out_of_range_text,
        schedule_list_page_underflow_text, schedule_list_pages, schedule_list_pages_global,
        validate_schedule, validate_schedule_at, ScheduleInput, IN_MINUTES_MIN,
    },
    scheduled_store::{
        audit_scheduled, count_scheduled_scoped, delete_scheduled, get_scheduled,
        list_scheduled_scoped, list_scheduled_window_scoped, put_scheduled,
        resolve_scheduled_id as resolve_scheduled_id_store, ScheduledAuditInput, ScheduledWrite,
        SCHEDULE_LIST_CAP,
    },
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{actor_id, new_id};
use crate::db_log::{db_class, discord_class, quota_write_class};

/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Schedule command failed; try again.";

/// Reply when the interaction arrives without a channel (pathological —
/// Discord always sends `channel_id` for guild slash commands). Copy of the
/// shared runtime's private constant: this module stays wiring-only so
/// `command_runtime.rs` keeps registration-line edits.
const NO_CHANNEL_REPLY: &str = "This command only works in a channel.";

// Schedule tests live in `command_runtime_tests.rs` alongside the feed
// slice's: they reuse its MockRest double, interaction builders and
// agent-testdb fixture, which cannot be imported from a submodule here.

/// `/schedule`: validate → upsert → audit → ephemeral confirmation.
/// Validation failures still audit (`rejected`): the id is minted before
/// validation, exactly like legacy, so the trail names the attempted row.
/// Malformed options refuse before any store read or audit via
/// [`schedule_options_strict`]: no row, no audit, one private correction.
/// `preview:true` renders the proposal privately instead and returns before
/// any id, store or audit effect; omitted or false keeps create unchanged.
pub(crate) async fn schedule_create(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let Some(channel_id) = interaction_channel(interaction) else {
        finish(executor, interaction, NO_CHANNEL_REPLY).await;
        return;
    };
    let (body, in_minutes, every_minutes) = match schedule_options_strict(interaction) {
        Ok(decoded) => decoded,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let preview = match schedule_preview_option_strict(interaction) {
        Ok(preview) => preview,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let now = now_millis();
    let absolute_due = match schedule_absolute_due(interaction, in_minutes, every_minutes, now) {
        Ok(due) => due,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };

    // The relative validator owns the unchanged body/mention and interval
    // checks. An already validated absolute one-shot supplies its minimum
    // delay only to select that branch; its due instant never uses the delay.
    // Pure: a preview resolves the same result without minting an attempt id
    // or auditing, while create keeps its id-first rejection trail below.
    let validated_result = validate_schedule(&ScheduleInput {
        body: body.as_deref().unwrap_or_default(),
        in_minutes: absolute_due.map(|_| IN_MINUTES_MIN).or(in_minutes),
        every_minutes,
    });
    if preview == Some(true) {
        match validated_result {
            Ok(validated) => {
                let due_ms =
                    absolute_due.unwrap_or_else(|| next_run_at_ms(now, validated.delay_minutes));
                finish(
                    executor,
                    interaction,
                    render_schedule_preview(
                        &channel_id,
                        due_ms,
                        validated.interval_seconds,
                        &validated.body,
                    ),
                )
                .await;
            }
            Err(err) => {
                finish(executor, interaction, err.to_string()).await;
            }
        }
        return;
    }

    let actor = actor_id(interaction);
    let id = new_id();
    let now_iso = format_iso_ms(now);

    let validated = match validated_result {
        Ok(validated) => validated,
        Err(err) => {
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some(&err.to_string()),
                &now_iso,
            )
            .await;
            finish(executor, interaction, err.to_string()).await;
            return;
        }
    };

    // The id is fresh, so this read is almost always empty — but like legacy
    // it decides the create/update audit action, the confirm verb, and the
    // creator passthrough on a replace.
    let existing = match get_scheduled(pool, guild_id, &id).await {
        Ok(existing) => existing,
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule read failed");
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some(db_class(&err)),
                &now_iso,
            )
            .await;
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let is_absolute = absolute_due.is_some();
    let next_run_at =
        format_iso_ms(absolute_due.unwrap_or_else(|| next_run_at_ms(now, validated.delay_minutes)));
    // Whole minutes by construction (`every-minutes * 60`), so this inverts
    // exactly; the confirm helper renders the recurring shape from it.
    let every_m = validated.interval_seconds.map(|s| s / 60);
    let write = ScheduledWrite {
        id: id.clone(),
        guild_id: guild_id.to_owned(),
        channel_id,
        body: validated.body,
        next_run_at: next_run_at.clone(),
        interval_seconds: validated.interval_seconds,
        enabled: true,
        created_by: existing
            .as_ref()
            .map(|row| row.created_by.clone())
            .unwrap_or_else(|| actor.clone()),
        created_at: existing
            .as_ref()
            .map(|row| row.created_at.clone())
            .unwrap_or_else(|| now_iso.clone()),
        updated_by: actor.clone(),
        updated_at: now_iso.clone(),
    };
    match put_scheduled(pool, &write).await {
        Ok(true) => {
            let created = existing.is_none();
            audit(
                pool,
                guild_id,
                &actor,
                if created {
                    "scheduled.create"
                } else {
                    "scheduled.update"
                },
                &id,
                "ok",
                None,
                &now_iso,
            )
            .await;
            finish(
                executor,
                interaction,
                schedule_confirmation(created, &id, every_m, &next_run_at, is_absolute),
            )
            .await;
        }
        Ok(false) => {
            // The id belongs to another guild (legacy guild-scoped no-match).
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some("id belongs to another guild"),
                &now_iso,
            )
            .await;
            finish(
                executor,
                interaction,
                "Scheduled message id belongs to another guild.",
            )
            .await;
        }
        Err(QuotaWriteError::Capacity(quota)) => {
            // Quota refusal audits exactly like a validation rejection: one
            // `scheduled.create`/`rejected` row with reason `quota`, naming
            // the fresh attempt id. Every refusal audits its own row
            // (1 refusal = 1 audit); nothing here dedupes or suppresses.
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some("quota"),
                &now_iso,
            )
            .await;
            finish(executor, interaction, quota.to_string()).await;
        }
        Err(err) => {
            tracing::warn!(reason = quota_write_class(&err), "schedule put failed");
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some(quota_write_class(&err)),
                &now_iso,
            )
            .await;
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// `/schedule-remove`: prefix-resolve the id → guild-scoped delete →
/// `scheduled.delete` audit (`ok` / `absent`) → ephemeral confirmation.
/// The success receipt confirms only that the definition and its future
/// runs are removed (see `schedule_removed_text`): a delivery the ticker
/// already admitted may still appear, and its stale-send cleanup outcome
/// belongs to the ticker, not to this reply. Zero or ambiguous prefix
/// matches refuse with the shared no-unique-match text and mutate nothing.
/// Malformed `id` options refuse before any store read or audit via
/// [`schedule_remove_option_strict`].
pub(crate) async fn schedule_remove(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let prefix = match schedule_remove_option_strict(interaction) {
        Ok(prefix) => prefix.unwrap_or_default(),
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    if prefix.is_empty() {
        finish(executor, interaction, no_unique_match_text(&prefix)).await;
        return;
    }
    if prefix
        .encode_utf16()
        .take(MAX_RESOURCE_ID_CHARS + 1)
        .count()
        > MAX_RESOURCE_ID_CHARS
    {
        finish(
            executor,
            interaction,
            "No unique scheduled message matches that id. Use the full id from /schedule-list.",
        )
        .await;
        return;
    }
    // Actor and timestamp sit above resolution so the resolve-failure
    // refusal audits with the same actor and clock as every other refusal.
    let actor = actor_id(interaction);
    let now_iso = format_iso_ms(now_millis());
    let resolved = match resolve_scheduled_id_store(pool, guild_id, &prefix).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule resolve failed");
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &prefix,
                "rejected",
                Some(db_class(&err)),
                &now_iso,
            )
            .await;
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let Some(id) = resolved else {
        finish(executor, interaction, no_unique_match_text(&prefix)).await;
        return;
    };
    // The receipt names the destination channel so staff can inspect it for
    // a delivery that was already admitted; capture it from the resolved row
    // before the delete, the way create reads its attempted row first. The
    // channel is receipt guidance only, so a failed read is logged and the
    // delete proceeds with generic destination guidance instead of gaining
    // a new refusal or audit outcome. A row that vanishes here simply falls
    // through to the delete's own `absent` outcome below.
    let channel_id = match get_scheduled(pool, guild_id, &id).await {
        Ok(row) => row.map(|row| row.channel_id),
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule remove read failed");
            None
        }
    };
    match delete_scheduled(pool, guild_id, &id).await {
        Ok(true) => {
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &id,
                "ok",
                None,
                &now_iso,
            )
            .await;
            finish(
                executor,
                interaction,
                schedule_removed_text(&id, channel_id.as_deref().unwrap_or_default()),
            )
            .await;
        }
        Ok(false) => {
            // Resolved but already gone (raced delete): legacy's `absent`.
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &id,
                "absent",
                None,
                &now_iso,
            )
            .await;
            finish(executor, interaction, no_such_schedule_text(&id)).await;
        }
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule delete failed");
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &id,
                "rejected",
                Some(db_class(&err)),
                &now_iso,
            )
            .await;
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// `/schedule-list`: guild-scoped list → ephemeral text. The first read is
/// capped at the earliest [`SCHEDULE_LIST_CAP`] rows; a full read counts
/// once and appends an explicit `(truncated, N more)` overflow line, or
/// `(truncated)` when the overflow count itself fails, so a cut is never
/// silent. Rows are ordered by (`next_run_at`, `id`). Over-cap guilds page
/// past the first window: `page` past the first window's packed pages reads
/// one bounded hidden window (`LIMIT/OFFSET`, still 25 rows) so ids through
/// the first 50 rows stay discoverable without reopening an unbounded read
/// (see `docs/schedule-queue-order.md`). A failed
/// hidden-window or overflow-count read fails closed with
/// `STORE_FAILURE_REPLY` instead of reporting a possibly-false out-of-range
/// page. A failed first, hidden-window, or overflow-count read audits one
/// `scheduled.list` / `rejected` row like the other store-failure refusals;
/// a successful list writes no audit row, and no list path writes schedule
/// rows. Both windows project the rows' current claim and delivery-fence
/// facts into each whole row before packing (see
/// `schedule_list_line_localized`); the read only ever observes those
/// facts, so a snapshot can go stale the moment it is rendered.
pub(crate) async fn schedule_list(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let page = match schedule_list_page_option_strict(interaction) {
        Ok(page) => page.unwrap_or(1),
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let here = match schedule_list_here_option_strict(interaction) {
        Ok(here) => here,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let channel_id = if here {
        let Some(channel_id) = interaction_channel(interaction) else {
            finish(executor, interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        Some(channel_id)
    } else {
        None
    };
    if page < 1 {
        let refusal = if here {
            "Page must be at least 1 for schedules in this channel."
        } else {
            schedule_list_page_underflow_text()
        };
        finish(executor, interaction, refusal).await;
        return;
    }
    match list_scheduled_scoped(pool, guild_id, channel_id.as_deref()).await {
        Ok(mut rows) => {
            rows.sort_by(|a, b| (&a.next_run_at, &a.id).cmp(&(&b.next_run_at, &b.id)));
            let mut lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    schedule_list_line_localized(
                        &row.id,
                        &row.channel_id,
                        &row.next_run_at,
                        row.interval_seconds,
                        row.enabled,
                        DeliveryHold::from_facts(
                            row.claim_token.is_some() || row.claimed_at.is_some(),
                            row.delivery_uncertain,
                        ),
                    )
                })
                .collect();
            // The first read is capped at SCHEDULE_LIST_CAP earliest rows.
            // A full read may hide more: count once and name the cut rows
            // so no id is silently dropped; a failed count still marks the
            // cut without a number. Under-cap guilds skip the second query
            // and render exactly as before.
            if rows.len() as i64 == SCHEDULE_LIST_CAP {
                let mut count_failure_class: Option<&str> = None;
                let total_opt =
                    match count_scheduled_scoped(pool, guild_id, channel_id.as_deref()).await {
                        Ok(total) => Some(total),
                        Err(err) => {
                            let class = db_class(&err);
                            tracing::warn!(reason = class, "schedule list count failed");
                            count_failure_class = Some(class);
                            None
                        }
                    };
                if let Some(overflow) =
                    schedule_list_overflow_for_count(total_opt, SCHEDULE_LIST_CAP)
                {
                    lines.push(overflow);
                }
                // Known overflow: pages past the first window's packed pages
                // read one bounded hidden window, so rows through the first
                // 50 stay reachable. Page 1 never pays for the extra query.
                // The first window's last page names the next page, and the
                // hidden window carries continuing global page numbers, so
                // no footer reads as the end of the list.
                if matches!(total_opt, Some(total) if total > SCHEDULE_LIST_CAP) {
                    let total = total_opt.expect("guarded by the known-overflow match above");
                    let first_pages = if here {
                        schedule_list_channel_pages(&lines, 0, true)
                    } else {
                        schedule_list_first_pages(&lines, true)
                    };
                    let first_total = first_pages.len() as i64;
                    if page > first_total {
                        match list_scheduled_window_scoped(
                            pool,
                            guild_id,
                            channel_id.as_deref(),
                            SCHEDULE_LIST_CAP,
                            SCHEDULE_LIST_CAP,
                        )
                        .await
                        {
                            Ok(mut hidden) => {
                                hidden.sort_by(|a, b| {
                                    (&a.next_run_at, &a.id).cmp(&(&b.next_run_at, &b.id))
                                });
                                let mut hidden_lines: Vec<String> = hidden
                                    .iter()
                                    .map(|row| {
                                        schedule_list_line_localized(
                                            &row.id,
                                            &row.channel_id,
                                            &row.next_run_at,
                                            row.interval_seconds,
                                            row.enabled,
                                            DeliveryHold::from_facts(
                                                row.claim_token.is_some()
                                                    || row.claimed_at.is_some(),
                                                row.delivery_uncertain,
                                            ),
                                        )
                                    })
                                    .collect();
                                let remaining = total - SCHEDULE_LIST_CAP - hidden.len() as i64;
                                if remaining > 0 {
                                    hidden_lines.push(schedule_list_overflow_line(remaining));
                                }
                                if hidden_lines.is_empty() {
                                    finish(
                                        executor,
                                        interaction,
                                        schedule_list_scoped_page(&lines, page, here),
                                    )
                                    .await;
                                    return;
                                }
                                let hidden_pages = if here {
                                    schedule_list_channel_pages(&hidden_lines, first_total, false)
                                } else {
                                    schedule_list_pages_global(&hidden_lines, first_total)
                                };
                                let hidden_total = hidden_pages.len() as i64;
                                let hidden_page = page - first_total;
                                if hidden_page <= hidden_total {
                                    finish(
                                        executor,
                                        interaction,
                                        hidden_pages[(hidden_page - 1) as usize].clone(),
                                    )
                                    .await;
                                } else {
                                    finish(
                                        executor,
                                        interaction,
                                        schedule_list_scoped_out_of_range(
                                            page,
                                            first_total + hidden_total,
                                            here,
                                        ),
                                    )
                                    .await;
                                }
                                return;
                            }
                            Err(err) => {
                                // Fail closed: the hidden rows may exist, so a
                                // failed window read must not report them as
                                // out of range. Mirrors the list-read failure
                                // path below, including its audit row.
                                tracing::warn!(
                                    reason = db_class(&err),
                                    "schedule list hidden window failed"
                                );
                                // A failed list names no schedule id, so the
                                // audit carries an empty target.
                                let actor = actor_id(interaction);
                                let now_iso = format_iso_ms(now_millis());
                                audit(
                                    pool,
                                    guild_id,
                                    &actor,
                                    "scheduled.list",
                                    "",
                                    "rejected",
                                    Some(db_class(&err)),
                                    &now_iso,
                                )
                                .await;
                                finish(executor, interaction, STORE_FAILURE_REPLY).await;
                                return;
                            }
                        }
                    }
                    finish(
                        executor,
                        interaction,
                        first_pages[(page - 1) as usize].clone(),
                    )
                    .await;
                    return;
                }
                if total_opt.is_none() {
                    // The overflow count failed, so the true page total is
                    // unknown: a page past the packed first window may or may
                    // not exist. Fail closed instead of reporting a
                    // possibly-false out-of-range. Mirrors the list-read
                    // failure path below, including its audit row; the count
                    // failure already warned above, so the audit reuses its
                    // captured error-class token as reason.
                    let first_total = if here {
                        schedule_list_channel_pages(&lines, 0, false).len()
                    } else {
                        schedule_list_pages(&lines).len()
                    } as i64;
                    if page > first_total {
                        // A failed list names no schedule id, so the audit
                        // carries an empty target.
                        let actor = actor_id(interaction);
                        let now_iso = format_iso_ms(now_millis());
                        audit(
                            pool,
                            guild_id,
                            &actor,
                            "scheduled.list",
                            "",
                            "rejected",
                            count_failure_class,
                            &now_iso,
                        )
                        .await;
                        finish(executor, interaction, STORE_FAILURE_REPLY).await;
                        return;
                    }
                }
            }
            finish(
                executor,
                interaction,
                schedule_list_scoped_page(&lines, page, here),
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule list failed");
            // A failed list names no schedule id, so the audit carries an
            // empty target; the success path still audits nothing.
            let actor = actor_id(interaction);
            let now_iso = format_iso_ms(now_millis());
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.list",
                "",
                "rejected",
                Some(db_class(&err)),
                &now_iso,
            )
            .await;
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

fn schedule_list_scoped_out_of_range(page: i64, total: i64, here: bool) -> String {
    if here {
        format!("Page {page} is out of range: this channel has {total} page(s) of schedules.")
    } else {
        schedule_list_page_out_of_range_text(page, total)
    }
}

fn schedule_list_scoped_page(lines: &[String], page: i64, here: bool) -> String {
    if !here {
        return schedule_list_page(lines, page);
    }
    let pages = schedule_list_channel_pages(lines, 0, false);
    if page > pages.len() as i64 {
        return schedule_list_scoped_out_of_range(page, pages.len() as i64, true);
    }
    pages[(page - 1) as usize].clone()
}

/// Localize only a validated stored instant; a corrupt legacy row must not
/// panic or invent a due time. Enrichment happens before whole-row packing.
fn localized_timestamp(next_run_at: &str) -> String {
    time::OffsetDateTime::parse(next_run_at, &time::format_description::well_known::Rfc3339)
        .map(|due| format!(" <t:{}:F>", due.unix_timestamp()))
        .unwrap_or_default()
}

fn schedule_delivery_hint(next_run_at: &str) -> String {
    let localized = localized_timestamp(next_run_at);
    if localized.is_empty() {
        " The 15s ticker and delivery may be delayed.".to_owned()
    } else {
        format!("{localized}. The 15s ticker and delivery may be delayed.")
    }
}

/// Claim evidence as a read snapshot. `claim_due` parks `next_run_at` at the
/// lease horizon, so the localized instant is named as that horizon, never
/// as a delivery time; a claim may also outlive the worker that took it, so
/// it proves no worker is running now. No claim token or timestamp is shown.
fn schedule_claimed_snapshot(next_run_at: &str) -> String {
    format!(
        " Snapshot: claimed (lease held){}; that time is a lease horizon, not a promised \
         delivery time. A claim does not prove a worker is running. Check the destination channel.",
        localized_timestamp(next_run_at)
    )
}

/// Delivery-fence evidence as a read snapshot. The ticker arms
/// `delivery_uncertain` before it sends, so the fence alone proves neither
/// that a post failed nor that the uncertainty is permanent: the post may
/// already be in the channel. No occurrence nonce is shown.
const SCHEDULE_FENCED_SNAPSHOT: &str = " Snapshot: delivery-fenced (uncertain). The fence is set before a send, so it does not prove the delivery failed or stays uncertain; the post may have appeared. Check the destination channel.";

/// Destination channel as data, not as row structure: only the snowflake
/// digits of a stored `channel_id` may reach a rendered row or receipt, so
/// a restored value carrying backticks, newlines or mention text cannot
/// split the whole-row line or inject a fake row. A value with no digits
/// renders empty rather than inventing a channel.
fn safe_channel_ref(channel_id: &str) -> String {
    channel_id.chars().filter(char::is_ascii_digit).collect()
}

/// `/schedule-remove` success receipt, bounded to what the delete actually
/// proved: the definition and its future runs are gone. A delivery the
/// ticker had already admitted when the delete landed is outside that
/// proof — it may still appear, and whether the ticker's stale-send
/// cleanup later succeeds is its own audited outcome, never promised
/// here. The caller reads the destination channel from the resolved row
/// before deleting it, so the inspection guidance names the channel the
/// admitted post went to.
fn schedule_removed_text(id: &str, channel_id: &str) -> String {
    let channel = safe_channel_ref(channel_id);
    let inspect = if channel.is_empty() {
        "check the destination channel".to_owned()
    } else {
        format!("check channel <#{channel}>")
    };
    format!(
        "Removed the definition for schedule `{id}`, so its future runs are removed. \
         A delivery that was already admitted may still appear; {inspect} to inspect \
         whether it posted."
    )
}

fn schedule_confirmation(
    created: bool,
    id: &str,
    every_minutes: Option<i64>,
    next_run_at: &str,
    include_delivery_hint: bool,
) -> String {
    let mut text = schedule_confirm_text(created, id, every_minutes, next_run_at);
    if include_delivery_hint {
        text.push_str(&schedule_delivery_hint(next_run_at));
    }
    text
}

/// Discord's message ceiling in UTF-16 units, shared with the body validator.
const PREVIEW_MAX_UNITS: usize = 2000;

/// Display-only cut marker. The body the staff member typed is untouched —
/// only this rendering is shortened — so the note names the cut explicitly
/// instead of silently dropping the tail.
const PREVIEW_TRUNCATED_NOTE: &str = "…(truncated for preview; create uses your full text)";

/// Render a proposed schedule without saving it: the invoking channel, the
/// first due instant in canonical UTC plus a Discord-local timestamp, the
/// one-off versus recurring shape, and the already-neutralized body as data.
///
/// Pure text in, text out: no id, no store read, no audit, no quota. The
/// caller runs the same validators as create first, so the due instant and
/// body shown here match what create would persist under the same clock —
/// but a later create still revalidates timing and availability, which this
/// reply says outright. It names no saved id, links no message and promises
/// no delivery. Metadata is never dropped to fit the cap; only the body is
/// cut, and only with the explicit truncation note above.
fn render_schedule_preview(
    channel_id: &str,
    due_ms: u64,
    interval_seconds: Option<i64>,
    body: &str,
) -> String {
    let repeats = match interval_seconds.map(|seconds| seconds / 60) {
        Some(every_minutes) => format!("every {every_minutes} minutes"),
        None => "one-off".to_owned(),
    };
    let head = format!(
        "Preview — not scheduled\nChannel: {channel_id}\nFirst run: {} (<t:{}:F>)\nRepeats: {repeats}\nBody: ",
        format_iso_ms(due_ms),
        due_ms / 1000,
    );
    let foot = "\nCreating this schedule later revalidates timing and availability; this preview reserves nothing.";
    let budget = PREVIEW_MAX_UNITS.saturating_sub(text_len(&head) + text_len(foot));
    let shown = if text_len(body) <= budget {
        body.to_owned()
    } else {
        let room = budget.saturating_sub(text_len(PREVIEW_TRUNCATED_NOTE));
        format!("{}{PREVIEW_TRUNCATED_NOTE}", truncate(body, room))
    };
    format!("{head}{shown}{foot}")
}

/// Delivery hold observed on one stored row, with precedence resolved at
/// construction: delivery-fenced beats claimed, so a row carrying both
/// facts renders its fence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryHold {
    None,
    Claimed,
    Fenced,
}

impl DeliveryHold {
    fn from_facts(claimed: bool, delivery_uncertain: bool) -> Self {
        if delivery_uncertain {
            Self::Fenced
        } else if claimed {
            Self::Claimed
        } else {
            Self::None
        }
    }
}

/// Render one list row with the delivery facts the store just read, before
/// whole-row packing. State precedence is deterministic: delivery-fenced
/// beats claimed, and both stay visible on disabled or restored rows (the
/// `(disabled)` marker from `schedule_list_line` still renders), so hold
/// evidence never disappears behind the enabled flag. Only an ordinary
/// enabled row with no hold carries the localized-due pending-delivery
/// hint; ordinary disabled rows still carry none. The snapshot wording on
/// hold rows names the observation and a safe next action without exposing
/// the claim token, occurrence nonce or body.
fn schedule_list_line_localized(
    id: &str,
    channel_id: &str,
    next_run_at: &str,
    interval_seconds: Option<i64>,
    enabled: bool,
    hold: DeliveryHold,
) -> String {
    let channel = safe_channel_ref(channel_id);
    let mut line = schedule_list_line(id, &channel, next_run_at, interval_seconds, enabled);
    match hold {
        DeliveryHold::Fenced => line.push_str(SCHEDULE_FENCED_SNAPSHOT),
        DeliveryHold::Claimed => line.push_str(&schedule_claimed_snapshot(next_run_at)),
        DeliveryHold::None if enabled => line.push_str(&schedule_delivery_hint(next_run_at)),
        DeliveryHold::None => {}
    }
    line
}

/// Decode and validate the optional absolute instant before any store or audit
/// effect. Legacy relative/recurring validation remains in `validate_schedule`.
fn schedule_absolute_due(
    interaction: &Interaction,
    in_minutes: Option<i64>,
    every_minutes: Option<i64>,
    now_ms: u64,
) -> Result<Option<u64>, String> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(None);
    };
    let mut options = data.options.iter().filter(|option| option.name == "at");
    let Some(option) = options.next() else {
        return Ok(None);
    };
    if options.next().is_some() {
        return Err(
            "Duplicate at option. Supply one RFC3339 timestamp with Z or a numeric offset."
                .to_owned(),
        );
    }
    let CommandOptionValue::String(at) = &option.value else {
        return Err(
            "Invalid at option. Supply an RFC3339 string timestamp with Z or a numeric offset."
                .to_owned(),
        );
    };
    if in_minutes.is_some() {
        return Err(
            "Choose either at or in-minutes, not both. Use at for a one-off RFC3339 time."
                .to_owned(),
        );
    }
    if every_minutes.is_some() {
        return Err("The at option is one-off only; do not combine it with every-minutes. Use in-minutes with every-minutes for recurring schedules.".to_owned());
    }
    validate_schedule_at(at, now_ms)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Current epoch millis, clamped non-negative for the ISO formatter.
fn now_millis() -> u64 {
    now_millis_for_test().max(0) as u64
}

/// Invoking channel as a snowflake string. `channel_id` is deprecated on the
/// wire but still the field Discord populates for guild slash commands; the
/// replacement `channel` object carries the id plus a payload we never read.
/// Mirrors the shared runtime's private helper for the same reason as
/// [`NO_CHANNEL_REPLY`].
#[allow(deprecated)]
fn interaction_channel(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.get().to_string())
        .or_else(|| interaction.channel_id.map(|id| id.get().to_string()))
}

/// Append one `scheduled.*` audit row; a write failure is logged but never
/// fails the operation it records (same posture as the sticky/feed slices).
#[allow(clippy::too_many_arguments)]
async fn audit(
    pool: &Pool<Postgres>,
    guild_id: &str,
    actor_id: &str,
    action: &str,
    target_key: &str,
    outcome: &str,
    reason: Option<&str>,
    now_iso: &str,
) {
    let input = ScheduledAuditInput {
        guild_id: guild_id.to_owned(),
        actor_id: Some(actor_id.to_owned()).filter(|actor| !actor.is_empty()),
        action: action.to_owned(),
        target_key: Some(target_key.to_owned()),
        outcome: outcome.to_owned(),
        reason: reason.map(str::to_owned),
    };
    if let Err(err) = audit_scheduled(pool, &input, now_iso, &new_id()).await {
        tracing::warn!(
            action,
            outcome,
            reason = db_class(&err),
            "schedule audit write failed"
        );
    }
}

/// Complete the original ephemeral defer, never issue a second callback.
async fn finish(executor: &ActionExecutor, interaction: &Interaction, content: impl AsRef<str>) {
    if let Err(err) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            content.as_ref(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "schedule reply edit failed");
    }
}

/// Decoded `/schedule` options: `body` (required string) + `in-minutes` /
/// `every-minutes` (optional integers). Missing values decode to `None`.
type ScheduleCreateOptions = (Option<String>, Option<i64>, Option<i64>);

/// Extract `/schedule`'s options: `body` (required string) + `in-minutes` /
/// `every-minutes` (optional integers). Missing values decode to `None`;
/// validation owns the refusal text.
///
/// Lenient legacy shape: wrong-typed and duplicated options are ignored or
/// last-wins. Test-only: handlers use [`schedule_options_strict`] instead,
/// and the pinned unit tests keep this compiling and GREEN.
#[cfg(test)]
pub(crate) fn schedule_options(interaction: &Interaction) -> ScheduleCreateOptions {
    let mut body = None;
    let mut in_minutes = None;
    let mut every_minutes = None;
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return (body, in_minutes, every_minutes);
    };
    for option in &data.options {
        match (option.name.as_str(), &option.value) {
            ("body", CommandOptionValue::String(value)) => body = Some(value.clone()),
            ("in-minutes", CommandOptionValue::Integer(value)) => in_minutes = Some(*value),
            ("every-minutes", CommandOptionValue::Integer(value)) => {
                every_minutes = Some(*value);
            }
            _ => {}
        }
    }
    (body, in_minutes, every_minutes)
}

/// `/schedule-remove`'s optional `id` option; absent and empty values are
/// refused before resolution, so they can never delete a schedule.
///
/// Lenient legacy shape: wrong-typed values decode to absent and duplicates
/// are first-wins. Test-only: handlers use [`schedule_remove_option_strict`]
/// instead, and the pinned unit tests keep this compiling and GREEN.
#[cfg(test)]
pub(crate) fn schedule_remove_option(interaction: &Interaction) -> Option<String> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    data.options
        .iter()
        .find_map(|option| match (option.name.as_str(), &option.value) {
            ("id", CommandOptionValue::String(value)) => Some(value.clone()),
            _ => None,
        })
}

/// Refusal literals for malformed schedule options. Each names the offending
/// option and carries a copyable corrected invocation, echoes no user input
/// (mention-safe), and stays far under Discord's 2,000-unit cap (bounded).
/// Handlers reply with these before any store read or audit write.
const SCHEDULE_INVALID_BODY: &str =
    "Invalid body option. Use /schedule body:<text> in-minutes:<minutes>.";
const SCHEDULE_DUPLICATE_BODY: &str =
    "Duplicate body option. Use /schedule body:<text> in-minutes:<minutes>.";
const SCHEDULE_INVALID_IN_MINUTES: &str =
    "Invalid in-minutes option. Use /schedule body:<text> in-minutes:<minutes>.";
const SCHEDULE_DUPLICATE_IN_MINUTES: &str =
    "Duplicate in-minutes option. Use /schedule body:<text> in-minutes:<minutes>.";
const SCHEDULE_INVALID_EVERY_MINUTES: &str =
    "Invalid every-minutes option. Use /schedule body:<text> every-minutes:<minutes>.";
const SCHEDULE_DUPLICATE_EVERY_MINUTES: &str =
    "Duplicate every-minutes option. Use /schedule body:<text> every-minutes:<minutes>.";
const SCHEDULE_INVALID_PREVIEW: &str =
    "Invalid preview option. Use /schedule body:<text> in-minutes:<minutes> preview:<true|false>.";
const SCHEDULE_DUPLICATE_PREVIEW: &str =
    "Duplicate preview option. Use /schedule body:<text> in-minutes:<minutes> preview:<true|false>.";
const SCHEDULE_REMOVE_INVALID_ID: &str =
    "Invalid id option. Use /schedule-remove id:<schedule-id>.";
const SCHEDULE_REMOVE_DUPLICATE_ID: &str =
    "Duplicate id option. Use /schedule-remove id:<schedule-id>.";
const SCHEDULE_LIST_INVALID_PAGE: &str =
    "Invalid page option. Use /schedule-list page:<number> with a whole number from 1.";
const SCHEDULE_LIST_DUPLICATE_PAGE: &str =
    "Duplicate page option. Use /schedule-list page:<number> with a whole number from 1.";

/// Strictly decode `/schedule`'s options. A present option with the wrong
/// wire type, or an option name sent twice, refuses with the literal naming
/// that option; name-count wins over type-check, and `body` wins over
/// `in-minutes` wins over `every-minutes`. Missing options decode to `None`
/// (validation owns those refusals) and unknown names stay ignored, so valid
/// invocations decode byte-identically to the lenient `schedule_options`.
pub(crate) fn schedule_options_strict(
    interaction: &Interaction,
) -> Result<ScheduleCreateOptions, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok((None, None, None));
    };
    let mut bodies: Vec<&CommandOptionValue> = Vec::new();
    let mut in_minutes_opts: Vec<&CommandOptionValue> = Vec::new();
    let mut every_minutes_opts: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        match option.name.as_str() {
            "body" => bodies.push(&option.value),
            "in-minutes" => in_minutes_opts.push(&option.value),
            "every-minutes" => every_minutes_opts.push(&option.value),
            _ => {}
        }
    }
    if bodies.len() > 1 {
        return Err(SCHEDULE_DUPLICATE_BODY);
    }
    if in_minutes_opts.len() > 1 {
        return Err(SCHEDULE_DUPLICATE_IN_MINUTES);
    }
    if every_minutes_opts.len() > 1 {
        return Err(SCHEDULE_DUPLICATE_EVERY_MINUTES);
    }
    let body = match bodies.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_INVALID_BODY),
    };
    let in_minutes = match in_minutes_opts.first() {
        None => None,
        Some(CommandOptionValue::Integer(value)) => Some(*value),
        Some(_) => return Err(SCHEDULE_INVALID_IN_MINUTES),
    };
    let every_minutes = match every_minutes_opts.first() {
        None => None,
        Some(CommandOptionValue::Integer(value)) => Some(*value),
        Some(_) => return Err(SCHEDULE_INVALID_EVERY_MINUTES),
    };
    Ok((body, in_minutes, every_minutes))
}

/// Strictly decode `/schedule`'s `preview` option. A wrong wire type or a
/// duplicated name refuses — even `false` twice, which would otherwise read
/// as an ordinary create — while a missing option decodes to `None` so
/// omitted preview keeps today's create behavior byte-identical.
pub(crate) fn schedule_preview_option_strict(
    interaction: &Interaction,
) -> Result<Option<bool>, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(None);
    };
    let mut previews: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        if option.name.as_str() == "preview" {
            previews.push(&option.value);
        }
    }
    if previews.len() > 1 {
        return Err(SCHEDULE_DUPLICATE_PREVIEW);
    }
    match previews.first() {
        None => Ok(None),
        Some(CommandOptionValue::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(SCHEDULE_INVALID_PREVIEW),
    }
}

/// Strictly decode `/schedule-remove`'s `id` option. A wrong wire type or a
/// duplicated name refuses; missing decodes to `None` (the empty-guard owns
/// that refusal) and unknown names stay ignored.
pub(crate) fn schedule_remove_option_strict(
    interaction: &Interaction,
) -> Result<Option<String>, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(None);
    };
    let mut ids: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        if option.name.as_str() == "id" {
            ids.push(&option.value);
        }
    }
    if ids.len() > 1 {
        return Err(SCHEDULE_REMOVE_DUPLICATE_ID);
    }
    match ids.first() {
        None => Ok(None),
        Some(CommandOptionValue::String(value)) => Ok(Some((*value).clone())),
        Some(_) => Err(SCHEDULE_REMOVE_INVALID_ID),
    }
}

/// Decode only the invoking-channel toggle, never a caller-supplied channel
/// or guild. A duplicate (even two false values) or wrong wire type refuses
/// before any list read or audit, without echoing untrusted values.
fn schedule_list_here_option_strict(interaction: &Interaction) -> Result<bool, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(false);
    };
    let mut options = data.options.iter().filter(|option| option.name == "here");
    let Some(option) = options.next() else {
        return Ok(false);
    };
    if options.next().is_some() {
        return Err(
            "Duplicate here option. Supply here once: /schedule-list here:true or here:false.",
        );
    }
    match &option.value {
        CommandOptionValue::Boolean(here) => Ok(*here),
        _ => Err("Invalid here option. Use a boolean: /schedule-list here:true or here:false."),
    }
}

/// Strictly decode `/schedule-list`'s `page` option. A wrong wire type or a
/// duplicated name refuses; missing decodes to `None` (page 1) and unknown
/// names stay ignored.
pub(crate) fn schedule_list_page_option_strict(
    interaction: &Interaction,
) -> Result<Option<i64>, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok(None);
    };
    let mut pages: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        if option.name.as_str() == "page" {
            pages.push(&option.value);
        }
    }
    if pages.len() > 1 {
        return Err(SCHEDULE_LIST_DUPLICATE_PAGE);
    }
    match pages.first() {
        None => Ok(None),
        Some(CommandOptionValue::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(SCHEDULE_LIST_INVALID_PAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_disabled_row_keeps_marker_without_pending_hint() {
        let line = schedule_list_line_localized(
            "row-1",
            "3333",
            "2030-01-01T00:00:00.000Z",
            Some(3600),
            false,
            DeliveryHold::Fenced,
        );
        assert!(line.contains("(disabled)"), "{line}");
        assert!(line.contains("delivery-fenced"), "{line}");
        assert!(!line.contains("delivery may be delayed"), "{line}");
        assert!(!line.to_lowercase().contains("pending"), "{line}");
    }

    #[test]
    fn claimed_row_labels_lease_horizon_and_unparsable_time_stays_clean() {
        let line = schedule_list_line_localized(
            "row-1",
            "3333",
            "not-a-time",
            None,
            true,
            DeliveryHold::Claimed,
        );
        assert!(line.contains("claimed (lease held)"), "{line}");
        assert!(
            line.contains("does not prove a worker is running"),
            "{line}"
        );
        assert!(!line.contains("<t:"), "{line}");
        assert!(!line.contains("delivery may be delayed"), "{line}");
    }

    #[test]
    fn ordinary_row_with_unparsable_time_keeps_single_spaced_hint() {
        let line = schedule_list_line_localized(
            "row-1",
            "3333",
            "not-a-time",
            None,
            true,
            DeliveryHold::None,
        );
        assert!(
            line.ends_with(" The 15s ticker and delivery may be delayed."),
            "{line}"
        );
        assert!(!line.contains("  "), "{line}");
    }

    #[test]
    fn hostile_channel_cannot_split_a_row_or_receipt() {
        let hostile = "1234`\n@everyone😀";
        assert_eq!(safe_channel_ref(hostile), "1234");
        let line = schedule_list_line_localized(
            "row-1",
            hostile,
            "2030-01-01T00:00:00.000Z",
            None,
            true,
            DeliveryHold::None,
        );
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(!line.contains("@everyone"), "{line}");
        let receipt = schedule_removed_text("row-1", hostile);
        assert!(receipt.contains("<#1234>"), "{receipt}");
        assert!(!receipt.contains("@everyone"), "{receipt}");
    }

    #[test]
    fn removed_receipt_without_channel_digits_uses_generic_destination() {
        let receipt = schedule_removed_text("row-1", "");
        assert!(
            receipt.contains("check the destination channel"),
            "{receipt}"
        );
        assert!(
            receipt.contains("already admitted may still appear"),
            "{receipt}"
        );
    }
}
