//! Protected acceptance: complete import diff privately reviewable.
//!
//! Admins reviewing a voice `/import` must be able to inspect every
//! proposed change instead of approving a preview that says `+N more`.
//! The compact 20-line preview and Confirm/Cancel UX stay; a bounded,
//! versioned full-review artifact (plain-text or JSON) rides the existing
//! ephemeral attachment path.
//!
//! Every test drives the real seams: `plan_import_preview` /
//! `plan_import_confirm` for pure planning plus `handle_voice_interaction_with`
//! through `import_harness` for dispatch/store/response. No Discord,
//! network, or database; the shared `Store`/`Http` doubles record writes.
//!
//! Builder contract (this file is immutable; see the JSON manifest on the
//! card):
//!
//! - Preview responses carry exactly one attachment with every changed
//!   path, complete old/new values, skipped-channel facts, and the
//!   content hash binding it to the preview buttons. Proposed applicable
//!   changes and skipped input are distinguishable sections.
//! - Summary, attachment and pending candidate/hash come from the SAME
//!   admitted snapshot. A stale Confirm that re-previews supplies fresh
//!   matching evidence.
//! - A documented conservative serialized-byte cap (recommended 32 KiB,
//!   must be `<= 64 KiB`) bounds the artifact. Over cap: truthful
//!   reduce-file/retry refusal, no Confirm buttons, no attachment, and
//!   pending state cleaned. Never silently truncate or confirm an
//!   unreviewable diff.
//! - Filename is fixed safe/versioned: contains `diff` and `v1`, ends in
//!   `.json` or `.txt`, carries the guild id, never upload text, no path
//!   separators. Mentions stay disabled on previews. No writes before a
//!   valid Confirm. Existing admin/guild/uploader, validation, pruning,
//!   hash, TTL, cancel/replay and CAS controls are unchanged.
//!
//! RED on the baseline: preview-with-attachment tests fail (no
//! attachment), the over-cap test fails (Preview instead of Refuse), and
//! stale re-preview fails (fresh preview without evidence). Controls
//! (no-change, skipped-only, absent/foreign, failures, CAS) pass on the
//! baseline and must stay green.

use super::*;
use two_bot_core::voice_config_diff::{diff_content_hash, DIFF_HASH_CHARS};

/// Conservative upper bound any Builder cap must respect. The recommended
/// cap is 32 KiB; tests use inputs whose artifacts are `<= 4 KiB`
/// (must preview) or `>= 100 KiB` (must refuse), so any cap in
/// `[4 KiB, 64 KiB]` satisfies both sides.
const ARTIFACT_CAP_CEILING: usize = 64 * 1024;
/// Under-cap artifacts in these tests stay below this.
const SMALL_ARTIFACT_MAX: usize = 16 * 1024;

fn attachments_of(response: &InteractionResponse) -> Option<Vec<Attachment>> {
    response
        .data
        .as_ref()
        .and_then(|data| data.attachments.clone())
}

fn attachment_of(response: &InteractionResponse) -> Attachment {
    let attachments = attachments_of(response).expect("preview carries a review attachment");
    assert_eq!(attachments.len(), 1, "exactly one review attachment");
    attachments.into_iter().next().unwrap()
}

fn assert_ephemeral(response: &InteractionResponse) {
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL),
        "response is ephemeral"
    );
}

fn assert_mentions_disabled(response: &InteractionResponse) {
    let mentions = response
        .data
        .as_ref()
        .and_then(|data| data.allowed_mentions.as_ref())
        .expect("preview disables mentions");
    assert!(mentions.parse.is_empty());
    assert!(mentions.users.is_empty());
    assert!(mentions.roles.is_empty());
}

fn assert_no_buttons(response: &InteractionResponse) {
    assert!(
        response
            .data
            .as_ref()
            .and_then(|data| data.components.as_ref())
            .is_none(),
        "no Confirm/Cancel buttons: {:?}",
        response_text(response)
    );
}

fn confirm_hash(confirm_id: &str) -> String {
    match parse_voice_custom_id(confirm_id) {
        Some(VoiceAction::ImportConfirm { hash, .. }) => hash,
        other => panic!("confirm button parses: {other:?}"),
    }
}

fn assert_safe_versioned_filename(filename: &str, guild: u64) {
    assert!(
        filename.contains("diff"),
        "filename names the diff: {filename:?}"
    );
    assert!(
        filename.contains("v1"),
        "filename is versioned: {filename:?}"
    );
    assert!(
        filename.ends_with(".json") || filename.ends_with(".txt"),
        "filename is plain-text or JSON: {filename:?}"
    );
    assert!(
        filename.contains(&guild.to_string()),
        "filename binds the guild: {filename:?}"
    );
    assert!(
        !filename.contains('/') && !filename.contains('\\') && !filename.contains(".."),
        "filename has no path separators: {filename:?}"
    );
    assert!(filename.len() <= 100, "filename is bounded: {filename:?}");
    assert!(
        filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "filename is safe chars only: {filename:?}"
    );
    // Never derived from upload text.
    assert!(!filename.contains("game-"), "{filename:?}");
    assert!(!filename.contains('@'), "{filename:?}");
}

fn many_aliases(base: &VoiceConfiguration, count: usize) -> VoiceConfiguration {
    let mut config = base.clone();
    for i in 0..count {
        config.aliases.push(config_codec::GameAlias {
            game: format!("game-{i:02}"),
            alias: format!("Alias {i:02}"),
        });
    }
    config
}

fn long_value_config(base: &VoiceConfiguration) -> (VoiceConfiguration, String) {
    let mut config = base.clone();
    // 100 chars: longest valid alias value, over the preview's 80-char clip.
    let long = format!("{}🎮🎮🎮🎮🎮", "z".repeat(95));
    assert_eq!(long.chars().count(), 100);
    config.aliases.push(config_codec::GameAlias {
        game: "long-game".to_owned(),
        alias: long.clone(),
    });
    (config, long)
}

fn mixed_config(base: &VoiceConfiguration) -> VoiceConfiguration {
    let mut config = base.clone();
    config.aliases.push(config_codec::GameAlias {
        game: "new-game".to_owned(),
        alias: "New Game".to_owned(),
    });
    config.creators.push(config_codec::CreatorConfiguration {
        channel_id: "999".to_owned(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    });
    config
}

/// Valid input under `MAX_IMPORT_BYTES` whose complete review artifact is
/// far over any conservative cap: 20 lists x 100 choices x ~94 chars.
fn huge_lists_config() -> VoiceConfiguration {
    let mut config = empty_config();
    for list in 0..20 {
        let choices = (0..100)
            .map(|choice| format!("choice-{list:02}-{choice:03}-{}", "x".repeat(80)))
            .collect();
        config.lists.push(config_codec::RandomList {
            name: format!("list-{list:02}"),
            choices,
        });
    }
    config
}

#[tokio::test]
async fn preview_attaches_complete_review_for_many_changes() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 25);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    assert!(bytes.len() < MAX_IMPORT_BYTES);
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let preview = preview.expect("preview");
    let text = response_text(&preview);

    // Compact summary stays: 20 lines plus a trailer.
    assert!(text.starts_with("Import preview: 25 changes"), "{text:?}");
    assert!(text.contains("+5 more"), "{text:?}");
    assert_ephemeral(&preview);
    assert_mentions_disabled(&preview);
    let (confirm_id, _) = preview_buttons(&preview);

    // The attachment carries every changed path completely.
    let attachment = attachment_of(&preview);
    assert_safe_versioned_filename(&attachment.filename, GUILD);
    assert!(!attachment.file.is_empty());
    assert!(
        attachment.file.len() <= SMALL_ARTIFACT_MAX,
        "small review fits the cap: {} bytes",
        attachment.file.len()
    );
    let body = String::from_utf8(attachment.file.clone()).expect("artifact is UTF-8");
    for i in 0..25 {
        assert!(
            body.contains(&format!("game-{i:02}")),
            "artifact omits game-{i:02}: {body:?}"
        );
        assert!(
            body.contains(&format!("Alias {i:02}")),
            "artifact omits Alias {i:02}"
        );
    }
    assert!(!body.contains("+5 more"), "artifact is complete: {body:?}");

    // No writes before Confirm.
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
    assert_eq!(confirm_hash(&confirm_id).len(), DIFF_HASH_CHARS);
}

#[tokio::test]
async fn artifact_contains_complete_long_and_astral_values() {
    let trace = Trace::default();
    let (incoming, long) = long_value_config(&empty_config());
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview);

    // Preview clips; the artifact must not.
    assert!(text.contains('…'), "{text:?}");
    assert!(!text.contains(&long), "{text:?}");
    let attachment = attachment_of(&preview);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    assert!(body.contains(&long), "artifact clips the value");
    assert!(body.contains("long-game"), "{body:?}");

    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn artifact_distinguishes_proposed_from_skipped() {
    let trace = Trace::default();
    let incoming = mixed_config(&full_config());
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.contains("new-game"), "{text:?}");
    assert!(text.contains("999"), "{text:?}");
    let (confirm_id, _) = preview_buttons(&preview);

    let attachment = attachment_of(&preview);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    let lower = body.to_lowercase();
    assert!(
        lower.contains("proposed") || lower.contains("applicable") || lower.contains("change"),
        "artifact names proposed changes: {body:?}"
    );
    assert!(
        lower.contains("skip"),
        "artifact names skipped input: {body:?}"
    );
    assert!(body.contains("new-game"), "{body:?}");
    assert!(body.contains("999"), "{body:?}");
    // The hash binds the artifact to the preview buttons.
    assert!(body.contains(&confirm_hash(&confirm_id)), "{body:?}");

    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn stale_confirm_supplies_fresh_review_evidence() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (stale_id, _) = preview_buttons(&preview.expect("preview"));

    // A concurrent change lands before Confirm.
    let mut concurrent = full_config();
    concurrent.aliases.push(config_codec::GameAlias {
        game: "go".to_owned(),
        alias: "Go".to_owned(),
    });
    *shared.lock().unwrap() = concurrent.clone();

    let confirm = component_interaction(&stale_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(owned);
    let fresh = response.expect("fresh preview");
    let text = response_text(&fresh);
    assert!(text.contains("Import preview:"), "{text:?}");
    assert!(!applied(&trace));
    assert_eq!(*shared.lock().unwrap(), concurrent);

    // Fresh preview carries fresh matching evidence.
    let (fresh_id, _) = preview_buttons(&fresh);
    assert_ne!(fresh_id, stale_id);
    let fresh_hash = confirm_hash(&fresh_id);
    let attachment = attachment_of(&fresh);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    assert!(body.contains(&fresh_hash), "{body:?}");
    assert_ephemeral(&fresh);
    assert_mentions_disabled(&fresh);

    // The fresh preview confirms cleanly.
    let confirm = component_interaction(&fresh_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert!(applied(&trace));
}

#[tokio::test]
async fn over_cap_diff_refuses_without_confirm_and_cleans_pending() {
    let trace = Trace::default();
    let incoming = huge_lists_config();
    let bytes = serde_json::to_vec(&incoming).unwrap();
    // Valid input under the import cap: the refusal is the review cap.
    assert!(
        bytes.len() < MAX_IMPORT_BYTES,
        "fixture must pass the input gate: {} bytes",
        bytes.len()
    );
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    assert!(runtime.pending_imports.lock().unwrap().is_empty());

    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    let text = response_text(&response);
    let lower = text.to_lowercase();
    assert!(
        lower.contains("reduce") || lower.contains("smaller") || lower.contains("too large"),
        "refusal names the correction: {text:?}"
    );
    assert!(
        lower.contains("retry") || lower.contains("again") || lower.contains("split"),
        "refusal names retry: {text:?}"
    );
    assert!(text.contains("Nothing was changed"), "{text:?}");
    // No actionable Confirm, no attachment, no pending state.
    assert_no_buttons(&response);
    assert!(
        attachments_of(&response).is_none(),
        "over-cap refusal carries no attachment"
    );
    assert!(
        runtime.pending_imports.lock().unwrap().is_empty(),
        "pending state cleaned on refusal"
    );
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
    // The refusal echoes no uploaded values.
    assert!(!text.contains("choice-00-000"), "{text:?}");
    assert!(response_text(&response).len() <= 2000);
}

#[tokio::test]
async fn artifact_filename_is_fixed_safe_and_versioned() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 3);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let attachment = attachment_of(&preview);
    assert_safe_versioned_filename(&attachment.filename, GUILD);
    assert!(
        attachment.file.len() <= ARTIFACT_CAP_CEILING,
        "artifact respects the documented ceiling"
    );
}

#[tokio::test]
async fn artifact_hash_matches_preview_buttons() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 5);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let (confirm_id, cancel_id) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);
    assert_eq!(hash.len(), DIFF_HASH_CHARS);
    // Cancel binds the same hash.
    match parse_voice_custom_id(&cancel_id) {
        Some(VoiceAction::ImportCancel {
            hash: cancel_hash, ..
        }) => assert_eq!(cancel_hash, hash),
        other => panic!("cancel parses: {other:?}"),
    }
    let attachment = attachment_of(&preview);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    assert!(body.contains(&hash), "artifact embeds the preview hash");
}

#[tokio::test]
async fn summary_artifact_and_candidate_share_one_snapshot() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 25);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.contains("25 changes"), "{text:?}");
    let (confirm_id, _) = preview_buttons(&preview);
    let hash = confirm_hash(&confirm_id);

    // The pending candidate hashes to the button hash against current state.
    let pending = runtime.pending_imports.lock().unwrap();
    let entry = pending
        .get(&(GUILD, UPLOADER, hash.clone()))
        .expect("pending candidate");
    let current = shared.lock().unwrap().clone();
    assert_eq!(diff_content_hash(&current, &entry.candidate), hash);
    drop(pending);

    // The artifact agrees: same count, same hash, all paths.
    let attachment = attachment_of(&preview);
    let body = String::from_utf8(attachment.file).expect("artifact is UTF-8");
    assert!(body.contains(&hash), "{body:?}");
    for i in 0..25 {
        assert!(body.contains(&format!("game-{i:02}")), "{body:?}");
    }
}

#[tokio::test]
async fn unsafe_mentions_stay_disabled_with_attachment() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.templates[0].name_template = "@everyone <@123> <#456>".to_owned();
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview);
    assert!(text.contains("@everyone"), "{text:?}");
    assert_mentions_disabled(&preview);
    assert_ephemeral(&preview);
    let attachment = attachment_of(&preview);
    assert_safe_versioned_filename(&attachment.filename, GUILD);
}

#[tokio::test]
async fn preview_hints_at_full_review_attachment() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 25);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, _) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let text = response_text(&preview).to_lowercase();
    assert!(
        text.contains("attach") || text.contains("full") || text.contains("review"),
        "preview hints at the full review: {text:?}"
    );
    assert!(attachments_of(&preview).is_some());
}

#[tokio::test]
async fn cancel_after_preview_clears_pending_without_write() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 5);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    assert!(attachments_of(&preview).is_some());
    let (confirm_id, cancel_id) = preview_buttons(&preview);
    assert!(!runtime.pending_imports.lock().unwrap().is_empty());

    let cancel = component_interaction(&cancel_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &cancel, Some(&inventory)).await;
    assert!(owned);
    assert!(response_text(&response.expect("cancelled")).contains("cancelled"));
    assert!(runtime.pending_imports.lock().unwrap().is_empty());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));

    // The consumed preview cannot be confirmed afterwards.
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("expired")).contains("expired"));
    assert!(!applied(&trace));
}

#[tokio::test]
async fn replayed_confirm_applies_once_with_review() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 3);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    assert!(attachments_of(&preview).is_some());
    let (confirm_id, _) = preview_buttons(&preview);

    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(applied_count(&trace), 1);

    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("expired")).contains("expired"));
    assert_eq!(applied_count(&trace), 1);
    assert_eq!(*shared.lock().unwrap(), incoming);
}

/// Each adjacent fixture adds exactly one unescaped ASCII byte to a list
/// choice. Keys, change count, and hash width stay fixed, so full serialization
/// grows one byte at a time without depending on JSON vs plain-text layout.
fn serialized_cap_fixture(padding: usize) -> VoiceConfiguration {
    const CHOICES: usize = 1000;
    assert!(padding <= 98 * CHOICES);
    let mut config = empty_config();
    for list in 0..10 {
        let choices = (0..100)
            .map(|choice| {
                let index = list * 100 + choice;
                let extra = padding / CHOICES + usize::from(index < padding % CHOICES);
                format!("{choice:02}{}", "x".repeat(extra))
            })
            .collect();
        config.lists.push(config_codec::RandomList {
            name: format!("list-{list:02}"),
            choices,
        });
    }
    config
}

async fn serialized_cap_response(padding: usize) -> InteractionResponse {
    let trace = Trace::default();
    let incoming = serialized_cap_fixture(padding);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    assert!(bytes.len() < MAX_IMPORT_BYTES, "not the upload cap");
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("preview or truthful cap refusal");
    assert_ephemeral(&response);
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
    if attachments_of(&response).is_some() {
        preview_buttons(&response);
        assert!(!runtime.pending_imports.lock().unwrap().is_empty());
    } else {
        assert_no_buttons(&response);
        assert!(runtime.pending_imports.lock().unwrap().is_empty());
    }
    response
}

/// Catch a cap applied to character count, a truncated artifact, or an
/// exclusive off-by-one boundary. Discover the admission edge through real
/// dispatch, then require the refusal to advertise that exact byte limit.
/// This leaves the serializer format and conservative cap value to the Builder.
#[tokio::test]
async fn serialized_review_cap_accepts_exact_bytes_and_refuses_one_over() {
    let small = serialized_cap_response(0).await;
    let small_bytes = attachment_of(&small).file.len();
    assert!(small_bytes <= ARTIFACT_CAP_CEILING);
    let large = serialized_cap_response(98_000).await;
    assert!(
        attachments_of(&large).is_none(),
        "complete data exceeds 64 KiB"
    );

    let mut low = 0;
    let mut high = 98_000;
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        let response = serialized_cap_response(middle).await;
        if attachments_of(&response).is_some() {
            low = middle;
        } else {
            high = middle;
        }
    }
    assert!(low > 0, "the review budget accommodates a small full diff");
    let exact = serialized_cap_response(low).await;
    let previous = serialized_cap_response(low - 1).await;
    let exact_attachment = attachment_of(&exact);
    let exact_bytes = exact_attachment.file.len();
    assert_eq!(exact_bytes, attachment_of(&previous).file.len() + 1);
    assert_eq!(
        exact_bytes,
        small_bytes + low,
        "no clipped values or size-dependent layout"
    );
    assert!(exact_bytes <= ARTIFACT_CAP_CEILING);

    let incoming = serialized_cap_fixture(low);
    let body = String::from_utf8(exact_attachment.file).expect("UTF-8 review");
    for list in &incoming.lists {
        assert!(body.contains(&list.name));
        for choice in &list.choices {
            assert!(body.contains(choice), "complete value at the byte boundary");
        }
    }

    // Adjacent admitted inputs differ by one serialized ASCII byte, not one
    // Unicode scalar or one input-document byte of whitespace.
    let over = serialized_cap_response(high).await;
    assert!(attachments_of(&over).is_none());
    let text = response_text(&over);
    let lower = text.to_lowercase();
    assert!(lower.contains("reduce") || lower.contains("smaller") || lower.contains("too large"));
    assert!(lower.contains("retry") || lower.contains("again") || lower.contains("split"));
    assert!(text.contains("Nothing was changed"));
    let numbers: Vec<_> = text.split(|c: char| !c.is_ascii_digit()).collect();
    assert!(
        numbers.contains(&exact_bytes.to_string().as_str()),
        "refusal reports the exact serialized-byte budget: {text:?}"
    );
}

/// A serializer that emits only added values would pass the many-alias test.
/// Pin complete old/new values and removals through the actual attachment.
#[tokio::test]
async fn artifact_includes_complete_changed_and_removed_values() {
    let trace = Trace::default();
    let mut current = full_config();
    current.aliases = vec![
        config_codec::GameAlias {
            game: "changed-game".to_owned(),
            alias: format!("{}🚀", "before-".repeat(13)),
        },
        config_codec::GameAlias {
            game: "removed-game".to_owned(),
            alias: "removed-value".to_owned(),
        },
    ];
    let old_value = current.aliases[0].alias.clone();
    let mut incoming = current.clone();
    incoming.aliases.truncate(1);
    incoming.aliases[0].alias = format!("{}🎮", "after-".repeat(15));
    let new_value = incoming.aliases[0].alias.clone();
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), current.clone(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let preview = preview.expect("preview");
    let body = String::from_utf8(attachment_of(&preview).file).expect("UTF-8 review");
    for value in [
        "changed-game",
        &old_value,
        &new_value,
        "removed-game",
        "removed-value",
    ] {
        assert!(
            body.contains(value),
            "artifact omits complete value {value:?}"
        );
    }
    assert_ephemeral(&preview);
    assert_mentions_disabled(&preview);
    assert_eq!(*shared.lock().unwrap(), current);
    assert!(!applied(&trace));
}

// --- Unchanged controls ---------------------------------------------------
// These pass on the baseline and must stay green after the Builder lands.

#[tokio::test]
async fn no_change_preview_has_no_confirm_or_artifact() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("notice");
    assert_eq!(response_text(&response), "No changes");
    assert_no_buttons(&response);
    assert!(attachments_of(&response).is_none());
    assert_ephemeral(&response);
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(!applied(&trace));
    assert!(runtime.pending_imports.lock().unwrap().is_empty());
}

#[tokio::test]
async fn skipped_only_reports_without_confirm() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.creators.push(config_codec::CreatorConfiguration {
        channel_id: "999".to_owned(),
        name_template: "{game}".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: config_codec::RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: config_codec::PermissionSource::Creator {},
    });
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), full_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let response = response.expect("notice");
    let text = response_text(&response);
    assert!(text.contains("unknown channel"), "{text}");
    assert!(text.contains("999"), "{text}");
    assert_no_buttons(&response);
    assert_eq!(*shared.lock().unwrap(), full_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn absent_uploader_refuses_import() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    // A Confirm click with no invoker identity is not the uploader. The
    // uploader check runs before the permission gate on clicks, so this
    // reaches the real binding refusal instead of a Manage Server reply.
    let mut anonymous = component_interaction(&confirm_id, manager(), UPLOADER);
    anonymous.member = None;
    anonymous.user = None;
    let (owned, response) = handle_import_capture(&runtime, &anonymous, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    assert!(
        response_text(&response).contains("Only the member"),
        "{:?}",
        response_text(&response)
    );
    assert!(attachments_of(&response).is_none());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));

    // The refusal changed nothing: the real uploader can still confirm.
    assert!(!runtime.pending_imports.lock().unwrap().is_empty());
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(response_text(&response.expect("applied")).starts_with("Import applied:"));
    assert_eq!(*shared.lock().unwrap(), full_config());
}

#[tokio::test]
async fn foreign_upload_guild_refuses_without_review_or_pending_state() {
    let trace = Trace::default();
    let mut incoming = full_config();
    incoming.guild_id = (GUILD + 1).to_string();
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("guild mismatch refusal");
    assert_no_buttons(&response);
    assert!(attachments_of(&response).is_none());
    assert_ephemeral(&response);
    assert!(runtime.pending_imports.lock().unwrap().is_empty());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn absent_or_foreign_interaction_guild_cannot_consume_preview() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));
    for guild in [None, Some(Id::new(GUILD + 1))] {
        let mut confirm = component_interaction(&confirm_id, manager(), UPLOADER);
        confirm.guild_id = guild;
        let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
        // The dispatcher may ignore a guildless/unowned interaction. If it
        // responds, it must not expose the bound review or actionable buttons.
        if let Some(response) = response {
            assert_no_buttons(&response);
            assert!(attachments_of(&response).is_none());
        }
        assert_eq!(*shared.lock().unwrap(), empty_config());
        assert!(!applied(&trace));
        assert!(!runtime.pending_imports.lock().unwrap().is_empty());
    }
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    assert!(
        response_text(&response.expect("bound uploader applies")).starts_with("Import applied:")
    );
    assert_eq!(applied_count(&trace), 1);
    assert_eq!(*shared.lock().unwrap(), full_config());
}

#[tokio::test]
async fn foreign_member_cannot_confirm() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));

    let foreign = component_interaction(&confirm_id, manager(), OTHER_MEMBER);
    let (owned, response) = handle_import_capture(&runtime, &foreign, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    assert!(
        response_text(&response).contains("Only the member"),
        "{:?}",
        response_text(&response)
    );
    assert!(attachments_of(&response).is_none());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn missing_inventory_refuses_preview_but_allows_cancel() {
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);

    // Preview without an inventory refuses.
    let (_, refused) = handle_import_capture(&runtime, &upload, None).await;
    assert!(response_text(&refused.expect("refusal")).contains("unavailable"));

    // A live preview still cancels without an inventory.
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (_, cancel_id) = preview_buttons(&preview.expect("preview"));
    let cancel = component_interaction(&cancel_id, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &cancel, None).await;
    assert!(owned);
    assert!(response_text(&response.expect("cancelled")).contains("cancelled"));
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn oversized_upload_refuses_before_download() {
    let trace = Trace::default();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), Vec::new());
    let inventory = config_inventory();
    let upload = import_interaction(MAX_IMPORT_BYTES as u64 + 1, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    let text = response_text(&response);
    assert!(text.contains("too large"), "{text}");
    assert!(text.contains(&MAX_IMPORT_BYTES.to_string()), "{text}");
    assert_no_buttons(&response);
    assert!(attachments_of(&response).is_none());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn malformed_upload_refuses_without_echo() {
    let trace = Trace::default();
    let bytes = b"{not json: untrusted-content".to_vec();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    let text = response_text(&response);
    assert!(text.contains("malformed"), "{text}");
    assert!(!text.contains("untrusted-content"), "{text}");
    assert_no_buttons(&response);
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn attachment_download_failure_refuses_without_preview() {
    let trace = Trace::default();
    let (runtime, shared) = import_harness(
        trace.clone(),
        empty_config(),
        vec![Err(RoomHttpError::UnknownOutcome)],
    );
    let inventory = config_inventory();
    let upload = import_interaction(100, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("refusal");
    assert!(
        response_text(&response).contains("Could not download"),
        "{:?}",
        response_text(&response)
    );
    assert_no_buttons(&response);
    assert!(attachments_of(&response).is_none());
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn preview_writes_nothing_before_confirm() {
    let trace = Trace::default();
    let incoming = many_aliases(&empty_config(), 10);
    let bytes = serde_json::to_vec(&incoming).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let before = shared.lock().unwrap().clone();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(preview.is_some());
    assert_eq!(*shared.lock().unwrap(), before);
    assert!(!applied(&trace));
    assert!(trace
        .lock()
        .unwrap()
        .iter()
        .all(|entry| entry != "config_apply"));
}

#[tokio::test]
async fn apply_uses_compare_and_swap() {
    // The store write lands only when storage still equals the snapshot
    // the preview was rendered from; otherwise Conflict. Pinning the
    // double's CAS plus the stale-button re-preview (attachment-agnostic).
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    *store.config.lock().unwrap() = empty_config();
    store
        .config_apply(GUILD, &full_config(), &empty_config())
        .await
        .expect("matching expected applies");
    assert_eq!(
        store
            .config_apply(GUILD, &empty_config(), &empty_config())
            .await,
        Err(StoreError::Conflict)
    );

    // Through dispatch: a stale button re-previews instead of overwriting.
    let trace = Trace::default();
    let bytes = serde_json::to_vec(&full_config()).unwrap();
    let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![Ok(bytes.clone())]);
    let inventory = config_inventory();
    let upload = import_interaction(bytes.len() as u64, manager(), UPLOADER);
    let (_, preview) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    let (confirm_id, _) = preview_buttons(&preview.expect("preview"));
    *shared.lock().unwrap() = full_config();
    let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
    let (_, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
    let text = response_text(&response.expect("fresh preview or notice"));
    assert!(
        text.contains("Import preview:") || text.contains("No changes") || text.contains("expired"),
        "{text:?}"
    );
    assert!(!applied(&trace));
}

#[test]
fn input_size_cap_is_exact_at_boundary() {
    // The existing input gate stays exact: MAX_IMPORT_BYTES previews (or
    // notices), one byte over refuses before parsing.
    let inventory = config_inventory();
    let current = empty_config();
    let valid = serde_json::to_vec(&full_config()).unwrap();
    assert!(valid.len() < MAX_IMPORT_BYTES);
    let mut exact = valid.clone();
    exact.resize(MAX_IMPORT_BYTES, b' ');
    assert_eq!(exact.len(), MAX_IMPORT_BYTES);
    assert!(matches!(
        plan_import_preview(&current, &exact, &inventory),
        ImportDecision::Preview { .. } | ImportDecision::Notice { .. }
    ));
    let mut over = exact.clone();
    over.push(b' ');
    match plan_import_preview(&current, &over, &inventory) {
        ImportDecision::Refuse { message } => {
            assert!(message.contains("too large"), "{message:?}");
            assert!(message.contains("Nothing was changed"), "{message:?}");
        }
        other => panic!("one byte over must refuse: {other:?}"),
    }
}

// --- myroom config-import refusal -------------------------------------------
// Config writes `voice_command_roles`; the runtime reads
// `voice_access_controls`. Until this import route enforces `myroom`, the
// codec must refuse it in parse/import, preview and forged Confirm, with no
// saved success and no writes. Established valid commands (e.g. `kick`) are
// the controls that still preview and confirm.

fn myroom_restricted_config() -> VoiceConfiguration {
    let mut incoming = empty_config();
    incoming.settings.command_roles = vec![config_codec::CommandRoles {
        command: "myroom".to_owned(),
        role_ids: vec![],
    }];
    incoming
}

fn kick_restricted_config() -> VoiceConfiguration {
    let mut incoming = empty_config();
    incoming.settings.command_roles = vec![config_codec::CommandRoles {
        command: "kick".to_owned(),
        role_ids: vec![GUILD.to_string()],
    }];
    incoming
}

#[test]
fn myroom_command_roles_preview_and_forged_confirm_refuse_in_planning() {
    let inventory = config_inventory();
    let current = empty_config();

    // Preview planning refuses `myroom`.
    let myroom_bytes = serde_json::to_vec(&myroom_restricted_config()).unwrap();
    match plan_import_preview(&current, &myroom_bytes, &inventory) {
        ImportDecision::Refuse { message } => {
            assert!(
                message.contains("settings.command_roles[0].command"),
                "myroom refusal names the field: {message:?}"
            );
            assert!(message.contains("Nothing was changed"), "{message:?}");
        }
        other => panic!("myroom preview must refuse, got {other:?}"),
    }

    // Forged Confirm planning refuses `myroom` with a dummy hash and with the
    // recomputed content hash: validation precedes the hash check, so no
    // forged candidate can apply. Refuse writes nothing by contract (only
    // Preview writes pending, only Apply touches the store).
    let candidate = myroom_restricted_config();
    for hash in [
        "0000000000000000".to_owned(),
        diff_content_hash(&current, &candidate),
    ] {
        match plan_import_confirm(&current, &candidate, &inventory, &hash) {
            ImportDecision::Refuse { message } => {
                assert!(
                    message.contains("settings.command_roles[0].command"),
                    "forged Confirm refuses myroom for {hash}: {message:?}"
                );
                assert!(message.contains("Nothing was changed"), "{message:?}");
            }
            other => panic!("forged Confirm with myroom must refuse, got {other:?}"),
        }
    }

    // Controls: established `kick` still previews and confirms.
    let kick_bytes = serde_json::to_vec(&kick_restricted_config()).unwrap();
    match plan_import_preview(&current, &kick_bytes, &inventory) {
        ImportDecision::Preview { .. } => {}
        other => panic!("kick preview must succeed, got {other:?}"),
    }
    let kick_candidate = kick_restricted_config();
    let kick_hash = diff_content_hash(&current, &kick_candidate);
    match plan_import_confirm(&current, &kick_candidate, &inventory, &kick_hash) {
        ImportDecision::Apply { .. } => {}
        other => panic!("kick Confirm must apply, got {other:?}"),
    }
}

#[tokio::test]
async fn myroom_command_roles_preview_refuses_without_write() {
    // Control first: established `kick` previews via dispatch with buttons
    // and attachment, but writes nothing before Confirm.
    {
        let trace = Trace::default();
        let kick_bytes = serde_json::to_vec(&kick_restricted_config()).unwrap();
        let (runtime, shared) =
            import_harness(trace.clone(), empty_config(), vec![Ok(kick_bytes.clone())]);
        let inventory = config_inventory();
        let upload = import_interaction(kick_bytes.len() as u64, manager(), UPLOADER);
        let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
        assert!(owned);
        let preview = response.expect("kick preview");
        let _ = preview_buttons(&preview);
        assert!(attachments_of(&preview).is_some());
        assert!(!runtime.pending_imports.lock().unwrap().is_empty());
        assert_eq!(*shared.lock().unwrap(), empty_config());
        assert!(!applied(&trace));
    }

    // `myroom` refuses via dispatch with no saved success and no writes: no
    // buttons, no attachment, no pending state, shared config untouched.
    let trace = Trace::default();
    let myroom_bytes = serde_json::to_vec(&myroom_restricted_config()).unwrap();
    let (runtime, shared) = import_harness(
        trace.clone(),
        empty_config(),
        vec![Ok(myroom_bytes.clone())],
    );
    let inventory = config_inventory();
    assert!(runtime.pending_imports.lock().unwrap().is_empty());
    let upload = import_interaction(myroom_bytes.len() as u64, manager(), UPLOADER);
    let (owned, response) = handle_import_capture(&runtime, &upload, Some(&inventory)).await;
    assert!(owned);
    let response = response.expect("myroom refusal");
    let text = response_text(&response);
    assert!(
        text.contains("settings.command_roles[0].command"),
        "myroom refusal names the field: {text:?}"
    );
    assert!(text.contains("Nothing was changed"), "{text:?}");
    assert!(!text.starts_with("Saved."), "no saved success: {text:?}");
    assert_no_buttons(&response);
    assert!(
        attachments_of(&response).is_none(),
        "refusal carries no attachment"
    );
    assert!(
        runtime.pending_imports.lock().unwrap().is_empty(),
        "pending state cleaned on refusal"
    );
    assert_eq!(*shared.lock().unwrap(), empty_config());
    assert!(!applied(&trace));
}

#[tokio::test]
async fn myroom_command_roles_forged_confirm_revalidates_before_any_write() {
    // A pending candidate from an older image or forged cache entry must not
    // bypass the codec refusal just because its hash and uploader are valid.
    for role_ids in [vec![], vec![GUILD.to_string()]] {
        let trace = Trace::default();
        let (runtime, shared) = import_harness(trace.clone(), empty_config(), vec![]);
        let inventory = config_inventory();
        let mut candidate = myroom_restricted_config();
        candidate.settings.command_roles[0].role_ids = role_ids;
        let hash = diff_content_hash(&empty_config(), &candidate);
        runtime.remember_pending_import(GUILD, UPLOADER, &hash, candidate);
        let confirm_id = import_confirm_custom_id(UPLOADER, &hash);
        let confirm = component_interaction(&confirm_id, manager(), UPLOADER);
        let (owned, response) = handle_import_capture(&runtime, &confirm, Some(&inventory)).await;
        assert!(owned);
        let response = response.expect("forged Confirm refusal");
        let text = response_text(&response);
        assert!(text.contains("settings.command_roles[0].command"), "{text}");
        assert!(text.contains("Nothing was changed"), "{text}");
        assert!(!text.starts_with("Saved."), "{text}");
        assert_ephemeral(&response);
        assert_no_buttons(&response);
        assert!(attachments_of(&response).is_none());
        assert!(runtime.pending_imports.lock().unwrap().is_empty());
        assert_eq!(*shared.lock().unwrap(), empty_config());
        assert!(!applied(&trace));
        assert!(
            trace.lock().unwrap().is_empty(),
            "Confirm performs no write"
        );
    }
}

/// Previous-reader compatibility for `/access restrict myroom`.
///
/// Baseline `89c4fba7e` did not list `myroom` in `VOICE_COMMANDS`, so its
/// strict validator refuses a `myroom` key as unknown. The current image
/// therefore never persists one: through the real `/access` dispatch,
/// `restrict myroom` is refused without a write, `unrestrict myroom` is a
/// truthful no-op, the unrelated restriction is untouched, and the actual
/// JSON round-trip of the runtime policy store (`voice_access_controls`)
/// stays acceptable to both the previous and the current validator. No
/// database or Discord.
#[tokio::test]
async fn myroom_access_restriction_is_refused_and_stays_previous_reader_compatible() {
    use std::collections::BTreeMap;
    use two_bot_core::voice_access::{validate_access_controls, AccessControls};

    // Frozen from baseline 89c4fba7e: `VOICE_COMMANDS` without `myroom`.
    const PREVIOUS_VOICE_COMMANDS: &[&str] = &[
        "alias",
        "alwaysprivate",
        "channelinfo",
        "create",
        "defaultlimit",
        "export",
        "group",
        "import",
        "inheritpermissions",
        "invite",
        "kick",
        "limit",
        "logging",
        "name",
        "nick",
        "ping",
        "position",
        "private",
        "public",
        "reclaim",
        "setup",
        "template",
        "templateassistant",
        "textchannels",
        "transfer",
        "unlimit",
    ];

    fn previous_validate(controls: &AccessControls) -> Result<(), String> {
        if controls.required_role.is_some_and(|role| role == 0) {
            return Err("role IDs must be nonzero".to_owned());
        }
        for (command, roles) in &controls.command_roles {
            if !PREVIOUS_VOICE_COMMANDS.contains(&command.as_str()) {
                return Err(format!("unknown voice command {command}"));
            }
            if roles.contains(&0) {
                return Err("role IDs must be nonzero".to_owned());
            }
        }
        Ok(())
    }

    // Production `voice_access_controls` JSON shape: `command_roles` as
    // `{command: [role_id_strings]}`. Encode and decode exactly as the store
    // does, so the validator sees what a previous reader would read.
    fn encode_runtime_policy(controls: &AccessControls) -> String {
        let map: BTreeMap<&str, Vec<String>> = controls
            .command_roles
            .iter()
            .map(|(command, roles)| {
                (
                    command.as_str(),
                    roles.iter().map(ToString::to_string).collect(),
                )
            })
            .collect();
        serde_json::to_string(&map).unwrap()
    }

    fn decode_runtime_policy(
        json: &str,
        room_creation_enabled: bool,
        required_role: Option<u64>,
    ) -> AccessControls {
        let stored: BTreeMap<String, Vec<String>> = serde_json::from_str(json).unwrap();
        let command_roles = stored
            .into_iter()
            .map(|(command, roles)| {
                let roles = roles
                    .into_iter()
                    .map(|role| role.parse::<u64>().unwrap())
                    .collect();
                (command, roles)
            })
            .collect();
        AccessControls {
            room_creation_enabled,
            required_role,
            command_roles,
        }
    }

    // Seed with an unrelated existing restriction.
    let shared = std::sync::Arc::new(std::sync::Mutex::new({
        let mut controls = AccessControls::default();
        controls.command_roles.insert("kick".to_owned(), vec![7]);
        controls
    }));
    let runtime = shared_runtime(Trace::default(), shared.clone(), None);

    // `/access restrict myroom` via the real dispatch is refused without a write.
    let restrict = access_interaction(
        sub_option(
            "restrict",
            vec![command_option("command", "myroom"), role_option("role", 8)],
        ),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &restrict).await;
    assert!(owned, "access stays owned");
    let text = response_text(&response.expect("restrict reply"));
    assert_eq!(
        text, "/myroom cannot be restricted with /access, so nothing was changed.",
        "new refuses myroom"
    );
    let saved = shared.lock().unwrap().clone();
    assert_eq!(saved.command_roles.len(), 1, "only kick: {saved:?}");
    assert_eq!(saved.command_roles.get("kick"), Some(&vec![7]));
    assert!(
        !saved.command_roles.contains_key("myroom"),
        "no myroom key: {saved:?}"
    );

    // `/access unrestrict myroom` finds nothing to remove and writes nothing.
    let unrestrict = access_interaction(
        sub_option("unrestrict", vec![command_option("command", "myroom")]),
        true,
    );
    let (owned, response) = handle_capture(&runtime, &unrestrict).await;
    assert!(owned, "access stays owned");
    let text = response_text(&response.expect("unrestrict reply"));
    assert_eq!(
        text, "/myroom has no role restriction, so nothing was changed.",
        "unrestrict is a truthful no-op"
    );
    assert_eq!(*shared.lock().unwrap(), saved, "unrestrict wrote nothing");

    // Actual JSON readback never carries myroom, so every reader accepts it.
    let json = encode_runtime_policy(&saved);
    assert!(!json.contains("myroom"), "JSON carries no myroom: {json:?}");
    assert!(json.contains("kick"), "JSON carries kick: {json:?}");
    let decoded = decode_runtime_policy(&json, saved.room_creation_enabled, saved.required_role);
    assert_eq!(decoded, saved, "JSON round-trip preserves runtime policy");
    assert!(
        previous_validate(&decoded).is_ok(),
        "previous reader compatible"
    );
    assert!(
        validate_access_controls(&decoded).is_ok(),
        "new reader accepts the policy"
    );
}
