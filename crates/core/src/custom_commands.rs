//! Framework-free custom-command validation, templates and outcome decisions.
//!
//! Ports legacy `src/automations/{template,service,gateway,discord,disable}.ts`
//! and `src/discord/commandNames.ts` (parity §1 #14–#16, #22–#23).
//! No Discord types, SQL or clock. FeatureGates owns environment parsing;
//! callers pass its booleans and lowercase admin-supplied names/triggers.
//!
//! Put/Delete decisions carry audit facts and registry-republish intent. Run
//! outcomes defer delivery and its success/failure audit to the shared REST
//! executor. The interaction router must enforce guild scope and ManageGuild
//! for admin commands. Neither router nor executor is reimplemented here.

use std::collections::HashSet;

use super::commands::{merge_commands, CommandDefinition, CustomCommand, GUILD_COMMAND_LIMIT};

/// Placeholders a custom-command template may reference (legacy
/// `TEMPLATE_PLACEHOLDERS`).
pub const TEMPLATE_PLACEHOLDERS: [&str; 4] = ["user", "username", "server", "channel"];

/// Stored-template bound in UTF-16 units (legacy JavaScript string.length).
pub const MAX_TEMPLATE_CHARS: usize = 2000;
/// Rendered-output bound, characters (legacy `MAX_RENDERED_CHARS` — Discord's
/// message ceiling re-checked after substitution).
pub const MAX_RENDERED_CHARS: usize = 2000;
/// Description bound, characters (legacy `validateCommandInput` 1–100).
pub const MAX_DESCRIPTION_CHARS: usize = 100;
/// Command-name bound, characters (legacy `NAME_PATTERN` `{1,32}`).
pub const MAX_COMMAND_NAME_CHARS: usize = 32;
/// Text-trigger bound, characters including the leading `!` (legacy
/// `TRIGGER_PATTERN` `^![a-z0-9_-]{1,32}$`: one `!` plus a command name).
pub const MAX_TEXT_TRIGGER_CHARS: usize = MAX_COMMAND_NAME_CHARS + 1;
/// Placeholder-extraction bound: `placeholders_in` returns at most this many
/// distinct keys in first-seen order, then stops scanning. Only four keys are
/// ever known, so any five distinct keys already contain an unknown one —
/// capping at 64 preserves `validate_template`'s unknown-placeholder refusal
/// exactly while keeping hostile inputs total-bounded in time and memory.
pub const MAX_PLACEHOLDERS: usize = 64;

/// Said to anybody who reaches a custom command while automations are off
/// (legacy `AUTOMATIONS_DISABLED_REPLY`; one constant shared with the router).
/// A row that exists while the feature is off gets this explicit refusal, not
/// silence: silence from a still-published command reads to Discord as "the
/// application failed to respond".
pub use crate::router::AUTOMATIONS_DISABLED_REPLY;

/// Template render context (legacy `TemplateContext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateContext {
    /// Invoking member's display mention (`<@id>`).
    pub user: String,
    /// Plain username, no mention ping.
    pub username: String,
    /// Guild name.
    pub server: String,
    /// Channel the command ran in.
    pub channel: String,
}

/// Template validation/render failure (legacy `validateTemplate` /
/// `renderTemplate` throws, as member-facing messages).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("Template must be between 1 and 2000 characters.")]
    InvalidLength,
    #[error(
        "Unknown placeholder {{{0}}}. Supported: {{user}}, {{username}}, {{server}}, {{channel}}."
    )]
    UnknownPlaceholder(String),
    #[error("Rendered command output is {0} characters; Discord's ceiling is 2000.")]
    RenderTooLong(usize),
}

/// Scan one `{key}` placeholder starting at `bytes[i] == b'{'`.
/// Returns the key end index (exclusive, past `}`) when the braces wrap a
/// lowercase-alpha run, else `None` — anything else is literal text, exactly
/// like legacy `/\{([a-z]+)\}/g` (so `{User}` is literal, never an error).
fn placeholder_end(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_lowercase() {
        j += 1;
    }
    if j > start && j < bytes.len() && bytes[j] == b'}' {
        Some(j + 1)
    } else {
        None
    }
}

/// Every placeholder the template references, deduplicated in first-seen
/// order (legacy `placeholdersIn`), capped at [`MAX_PLACEHOLDERS`] distinct
/// keys so hostile inputs stay total-bounded.
#[must_use]
pub fn placeholders_in(template: &str) -> Vec<String> {
    let bytes = template.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if found.len() >= MAX_PLACEHOLDERS {
            break;
        }
        if bytes[i] == b'{' {
            if let Some(end) = placeholder_end(bytes, i) {
                let key = template[i + 1..end - 1].to_owned();
                if !found.contains(&key) {
                    found.push(key);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Validate at definition time: length bounds plus unknown-placeholder
/// refusal (legacy `validateTemplate` — a typo'd `{usre}` must fail here,
/// never render literally in front of members).
pub fn validate_template(template: &str) -> Result<(), TemplateError> {
    let len = template.encode_utf16().count();
    if !(1..=MAX_TEMPLATE_CHARS).contains(&len) {
        return Err(TemplateError::InvalidLength);
    }
    if let Some(unknown) = placeholders_in(template)
        .iter()
        .find(|p| !TEMPLATE_PLACEHOLDERS.contains(&p.as_str()))
    {
        return Err(TemplateError::UnknownPlaceholder(unknown.clone()));
    }
    Ok(())
}

/// Render a validated template against `ctx` (legacy `renderTemplate`).
/// Unknown-but-pattern-matching keys render literally (unreachable after
/// validation, kept so render never invents text); output past Discord's
/// ceiling is refused. The render refuses once buffered bytes breach a byte
/// tripwire (`3 *` the ceiling: triple the UTF-16 length upper-bounds byte length, so
/// past the tripwire the output is certainly over-ceiling), materialising at
/// most one expansion past it. Outputs within the tripwire render exactly as
/// before, keeping exact lengths; tripwire refusals report buffered bytes,
/// always past the ceiling.
pub fn render_template(template: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
    let bytes = template.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = placeholder_end(bytes, i) {
                let key = &template[i + 1..end - 1];
                let text = match key {
                    "user" => ctx.user.as_str(),
                    "username" => ctx.username.as_str(),
                    "server" => ctx.server.as_str(),
                    "channel" => ctx.channel.as_str(),
                    _ => &template[i..end],
                };
                if out.len() > MAX_RENDERED_CHARS * 3 {
                    return Err(TemplateError::RenderTooLong(out.len()));
                }
                out.push_str(text);
                i = end;
                continue;
            }
        }
        // Advance one char (templates are validated UTF-8 by construction).
        let ch = template[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    let len = out.encode_utf16().count();
    if len > MAX_RENDERED_CHARS {
        return Err(TemplateError::RenderTooLong(len));
    }
    Ok(out)
}

/// A stored custom-command definition (legacy `AutomationCommandRow`, command
/// columns only; audit lives in [`AuditRecord`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCommand {
    pub guild_id: String,
    pub name: String,
    pub description: String,
    pub template: String,
    /// Optional `!trigger` text form; `None` = slash-only.
    pub text_trigger: Option<String>,
    pub enabled: bool,
}

impl StoredCommand {
    /// Registry entry for the published set (parity #22: dynamic commands
    /// join the registry; disabled rows never publish — `merge_commands`
    /// filters on `enabled`).
    #[must_use]
    pub fn registry_entry(&self) -> CustomCommand {
        CustomCommand {
            name: self.name.clone(),
            description: self.description.clone(),
            enabled: self.enabled,
        }
    }
}

/// Admin-supplied definition for `/command` (legacy `PutCommandInput`;
/// `description` already defaulted by the caller, name/trigger already
/// lowercased by the Discord layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutCommandInput {
    pub name: String,
    pub description: String,
    pub template: String,
    pub text_trigger: Option<String>,
}

/// Custom-command validation refusal (legacy `validateCommandInput` throws +
/// `CommandCapacityError`, as member-facing messages; `AutomationsDisabled`
/// is the `enabled: false` handler refusal).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("Command name must match ^[a-z0-9_-]{{1,32}}$.")]
    InvalidName,
    #[error("Command name \"{}\" is reserved by Owen.", escape_inspect_text(.0))]
    ReservedName(String),
    #[error("Description must be between 1 and 100 characters.")]
    InvalidDescription,
    #[error("{0}")]
    InvalidTemplate(#[from] TemplateError),
    #[error("Text trigger must match ^![a-z0-9_-]{{1,32}}$.")]
    InvalidTextTrigger,
    #[error("Text trigger must be lowercase.")]
    TextTriggerNotLowercase,
    #[error("Text trigger \"{}\" is reserved by Owen.", escape_inspect_text(.0))]
    ReservedTextTrigger(String),
    #[error("Write would define more than {0} custom commands, but that is the guild limit.")]
    OverCapacity(usize),
    #[error("Automations are disabled on this server.")]
    AutomationsDisabled,
}

/// Short stable code per refusal for audit `reason` (legacy `safeErrorName`).
#[must_use]
pub fn error_code(err: &CommandError) -> &'static str {
    match err {
        CommandError::InvalidName => "invalid_name",
        CommandError::ReservedName(_) => "reserved_name",
        CommandError::InvalidDescription => "invalid_description",
        CommandError::InvalidTemplate(_) => "invalid_template",
        CommandError::InvalidTextTrigger => "invalid_text_trigger",
        CommandError::TextTriggerNotLowercase => "text_trigger_not_lowercase",
        CommandError::ReservedTextTrigger(_) => "reserved_text_trigger",
        CommandError::OverCapacity(_) => "over_capacity",
        CommandError::AutomationsDisabled => "automations_disabled",
    }
}

use super::leveling::{valid_command_name as valid_name, valid_text_trigger as valid_trigger};

/// Validate one `/command` definition: name shape, builtin collision,
/// description/template bounds, trigger shape (legacy `validateCommandInput`).
pub fn validate_put_input(
    input: &PutCommandInput,
    builtins: &HashSet<String>,
) -> Result<(), CommandError> {
    if !valid_name(&input.name) {
        return Err(CommandError::InvalidName);
    }
    if builtins.contains(&input.name) {
        return Err(CommandError::ReservedName(input.name.clone()));
    }
    let desc_len = input.description.encode_utf16().count();
    if !(1..=MAX_DESCRIPTION_CHARS).contains(&desc_len) {
        return Err(CommandError::InvalidDescription);
    }
    validate_template(&input.template)?;
    if let Some(trigger) = &input.text_trigger {
        if !valid_trigger(trigger) {
            return Err(CommandError::InvalidTextTrigger);
        }
        if trigger != &trigger.to_lowercase() {
            return Err(CommandError::TextTriggerNotLowercase);
        }
    }
    Ok(())
}

/// Guild command ceiling left for admin-defined commands after every Owen
/// builtin is reserved (legacy `MAX_CUSTOM_COMMANDS`).
#[must_use]
pub fn max_custom_commands(builtin_reserved: usize) -> usize {
    GUILD_COMMAND_LIMIT.saturating_sub(builtin_reserved)
}

/// Capacity gate: same-name updates always fit; a new name past the guild
/// limit is refused (legacy `putCommand` capacity check).
pub fn check_capacity(
    existing_count: usize,
    is_new_name: bool,
    builtin_reserved: usize,
) -> Result<(), CommandError> {
    let max = max_custom_commands(builtin_reserved);
    if is_new_name && existing_count >= max {
        return Err(CommandError::OverCapacity(max));
    }
    Ok(())
}

/// Every name Owen owns, derived from the command definitions so a new
/// builtin can never become shadowable because somebody forgot a second
/// handwritten list (legacy `BUILTIN_COMMAND_NAMES`). Rota is absent: the
/// staging-only `/rota-acknowledge` was dropped with the rota stack (matrix
/// §9 drop 1), so its name is NOT reserved in the port.
#[must_use]
pub fn builtin_command_names() -> HashSet<String> {
    super::commands::core_commands()
        .iter()
        .chain(super::feature_commands::feature_commands().iter())
        .chain(super::moderation::moderation_commands().iter())
        .map(|d| d.name.clone())
        .collect()
}

/// All-enabled publication slots occupied by builtins, independent of the
/// current feature gates. Reserve names separately: the unpublished assistant
/// owns its name but consumes no Discord command slot. Voice `/kick` overlaps
/// moderation and therefore counts only once.
#[must_use]
pub fn published_builtin_count() -> usize {
    let mut names = builtin_command_names();
    names.extend(
        super::voice_rooms::voice_commands()
            .into_iter()
            .map(|def| def.name),
    );
    names.len()
}

/// Names a custom command may not be *written* under: every builtin plus the
/// voice-room and assistant sets. The voice sink dispatches by name, so a custom
/// command named `setup` or `export` would reach the voice handlers the moment `TWO_VOICE`
/// turns on, even if it was created while voice was off. This guards writes
/// only (`/command`, imports). Dispatch keeps [`builtin_command_names`], so a
/// stored row that predates the reservation keeps running while voice is off.
#[must_use]
pub fn reserved_command_names() -> HashSet<String> {
    let mut names = builtin_command_names();
    names.extend(
        super::voice_rooms::voice_commands()
            .into_iter()
            .chain(super::voice_assistant::assistant_commands())
            .map(|def| def.name),
    );
    names
}

/// Refuse unless automations are on. Registered with `false`, the handler
/// refuses instead of executing — the half of disable a deregister cannot
/// cover, because an interaction can already be in flight when the DELETE
/// lands (legacy `registerAutomationCommands({enabled: false})`, TOG-3189).
pub fn require_automations_enabled(enabled: bool) -> Result<(), CommandError> {
    if enabled {
        Ok(())
    } else {
        Err(CommandError::AutomationsDisabled)
    }
}

/// One audit fact (legacy `AutomationAuditInput`): ids and outcomes, never
/// message content. `actor_id` is `None` for system actors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub guild_id: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub target_key: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
}

fn bounded_audit_name(name: &str) -> String {
    name.chars().take(MAX_COMMAND_NAME_CHARS).collect()
}

impl AuditRecord {
    #[must_use]
    pub fn put(guild_id: &str, actor_id: &str, name: &str, created: bool) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: (if created {
                "command.create"
            } else {
                "command.update"
            })
            .to_owned(),
            target_key: Some(bounded_audit_name(name)),
            outcome: "ok".to_owned(),
            reason: None,
        }
    }

    #[must_use]
    pub fn put_rejected(
        guild_id: &str,
        actor_id: &str,
        name: &str,
        existing: bool,
        err: &CommandError,
    ) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: (if existing {
                "command.update"
            } else {
                "command.create"
            })
            .to_owned(),
            target_key: Some(bounded_audit_name(name)),
            outcome: "rejected".to_owned(),
            reason: Some(error_code(err).to_owned()),
        }
    }

    #[must_use]
    pub fn delete(guild_id: &str, actor_id: &str, name: &str, deleted: bool) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: "command.delete".to_owned(),
            target_key: Some(bounded_audit_name(name)),
            outcome: (if deleted { "ok" } else { "absent" }).to_owned(),
            reason: None,
        }
    }

    /// One content-free fact per actual pause/resume transition. The action
    /// names the saved state (`command.disable` / `command.enable`), never
    /// removal or a run: pausing is not deletion and must not mint
    /// `command.run` / `command.text_attempt` facts.
    #[must_use]
    pub fn toggle(guild_id: &str, actor_id: &str, name: &str, enabled: bool) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: (if enabled {
                "command.enable"
            } else {
                "command.disable"
            })
            .to_owned(),
            target_key: Some(bounded_audit_name(name)),
            outcome: "ok".to_owned(),
            reason: None,
        }
    }

    #[must_use]
    pub fn run(guild_id: &str, actor_id: &str, name: &str, ok: bool, reason: Option<&str>) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: "command.run".to_owned(),
            target_key: Some(bounded_audit_name(name)),
            outcome: (if ok { "ok" } else { "failed" }).to_owned(),
            reason: reason.map(str::to_owned),
        }
    }
}

/// Outcome of a validated `/command` write: what changed, its audit row, and
/// whether the caller must republish the merged registry (legacy
/// `syncCommands?.()` after every definition change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutDecision {
    pub created: bool,
    pub audit: AuditRecord,
    pub resync_registry: bool,
}

/// Adjudicate a validated write against the stored row (legacy
/// `AutomationService::putCommand` success path).
#[must_use]
pub fn adjudicate_put(
    guild_id: &str,
    actor_id: &str,
    name: &str,
    existing: Option<&StoredCommand>,
) -> PutDecision {
    PutDecision {
        created: existing.is_none(),
        audit: AuditRecord::put(guild_id, actor_id, name, existing.is_none()),
        resync_registry: true,
    }
}

/// Outcome of a `/command-remove`: whether a row went away, its audit row,
/// and whether the caller must republish (only when something was actually
/// removed — legacy `syncCommands?.()` runs only on `gone`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteDecision {
    pub deleted: bool,
    pub audit: AuditRecord,
    pub resync_registry: bool,
}

/// Adjudicate a `/command-remove` against the delete result (legacy
/// `AutomationService::deleteCommand`: `ok` vs `absent`).
#[must_use]
pub fn adjudicate_delete(
    guild_id: &str,
    actor_id: &str,
    name: &str,
    deleted: bool,
) -> DeleteDecision {
    DeleteDecision {
        deleted,
        audit: AuditRecord::delete(guild_id, actor_id, name, deleted),
        resync_registry: deleted,
    }
}

/// Outcome of a `/command-disable` / `/command-enable` request against the
/// stored row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// The enabled flag flipped; the caller must republish the registry.
    Changed,
    /// The row already held the requested state: an explicit no-op, never an
    /// implicit registry repair.
    Unchanged,
    /// No such row in this guild: a zero-write refusal, never an upsert.
    Missing,
}

/// Outcome of a pause/resume request: the outcome, the resulting (or, for a
/// no-op or miss, the requested) enabled state, the audit row when a
/// transition committed, and whether the caller must republish the merged
/// registry (only on a real flip — legacy `syncCommands?.()` runs only when
/// the set actually changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleDecision {
    pub outcome: ToggleOutcome,
    pub enabled: bool,
    pub audit: Option<AuditRecord>,
    pub resync_registry: bool,
}

/// Adjudicate a pause/resume request against the stored row. Missing rows
/// refuse without writing, current-state requests are explicit no-ops, and
/// only a real flip audits and republishes.
#[must_use]
pub fn adjudicate_toggle(
    guild_id: &str,
    actor_id: &str,
    name: &str,
    existing: Option<&StoredCommand>,
    enabled: bool,
) -> ToggleDecision {
    match existing {
        None => ToggleDecision {
            outcome: ToggleOutcome::Missing,
            enabled,
            audit: None,
            resync_registry: false,
        },
        Some(row) if row.enabled == enabled => ToggleDecision {
            outcome: ToggleOutcome::Unchanged,
            enabled,
            audit: None,
            resync_registry: false,
        },
        Some(_) => ToggleDecision {
            outcome: ToggleOutcome::Changed,
            enabled,
            audit: Some(AuditRecord::toggle(guild_id, actor_id, name, enabled)),
            resync_registry: true,
        },
    }
}

/// Validate a stored definition for re-enable: the same write rules as
/// `/command` (name shape, write reservation, description/template/trigger
/// bounds) plus the dispatch rule that a stored trigger must not shadow a
/// builtin name. Capacity is intentionally unchecked: resuming an existing
/// row never grows the guild past its ceiling, and rival-trigger conflicts
/// are impossible — the unique trigger index forbids two rows from holding
/// one trigger, so a conflicting seed cannot exist to re-enable.
///
/// The name follows the write-reservation set while the trigger follows the
/// dispatch set: a grandfathered voice-named trigger still fires while voice
/// is off, but a builtin-named trigger can never fire (`is_builtin_trigger`
/// excludes it), so enabling such a row would claim to restore dead text.
pub fn validate_toggle_enable(
    row: &StoredCommand,
    reserved: &HashSet<String>,
    dispatch: &HashSet<String>,
) -> Result<(), CommandError> {
    validate_put_input(
        &PutCommandInput {
            name: row.name.clone(),
            description: row.description.clone(),
            template: row.template.clone(),
            text_trigger: row.text_trigger.clone(),
        },
        reserved,
    )?;
    if let Some(trigger) = row.text_trigger.as_deref() {
        if is_builtin_trigger(trigger, dispatch) {
            return Err(CommandError::ReservedTextTrigger(trigger.to_owned()));
        }
    }
    Ok(())
}

/// Outcome of a dynamic `/<custom>` invocation (legacy `InteractionCreate`
/// custom branch + `AutomationService::runCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Render and deliver `template` (audited `command.run ok/failed` by the
    /// executor around delivery).
    Render { name: String, template: String },
    /// Automations are off: reply [`AUTOMATIONS_DISABLED_REPLY`], never
    /// execute.
    RefusedDisabled,
    /// Unknown name, or a row disabled after publication: the router stays
    /// silent (some other application's command; not ours to answer).
    Ignored,
}

/// Adjudicate a dynamic invocation: refusal while disabled, silence for
/// unknown/disabled rows, render otherwise.
#[must_use]
pub fn adjudicate_run(automations_enabled: bool, row: Option<&StoredCommand>) -> RunOutcome {
    match row {
        Some(cmd) if automations_enabled && cmd.enabled => RunOutcome::Render {
            name: cmd.name.clone(),
            template: cmd.template.clone(),
        },
        Some(_) if !automations_enabled => RunOutcome::RefusedDisabled,
        _ => RunOutcome::Ignored,
    }
}

/// Everything after the leading `!` up to the first whitespace, lowercased
/// (legacy `triggerWord`). Reads only the first token, in memory; `None`
/// when the content is not a trigger at all.
#[must_use]
pub fn trigger_word(content: &str) -> Option<String> {
    let rest = content.strip_prefix('!')?;
    let word: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if word.is_empty() {
        None
    } else {
        Some(format!("!{}", word.to_lowercase()))
    }
}

/// Whether a `!` first token names a builtin (without its `!`): builtin
/// names are excluded from text triggers (parity #23).
#[must_use]
pub fn is_builtin_trigger(word: &str, builtins: &HashSet<String>) -> bool {
    word.strip_prefix('!')
        .is_some_and(|name| builtins.contains(name))
}

/// The ordinary message path's explicit moderation decision. A MessageCreate
/// event, successful funnel capture, or a message surviving deletion is NOT an
/// acceptance decision. Reuse the inspection result; do not inspect twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationMessageAcceptance {
    /// The configured moderation service is deliberately disabled (not missing).
    AutomodDisabled,
    /// Inspection completed without a match.
    Unmatched,
    /// The moderation policy explicitly exempts this channel/member.
    Exempt,
    /// Includes dry-run matches, protected-author refusals and matched errors.
    Matched,
    /// Missing service/result or unknown inspection error: fail closed.
    Unavailable,
    /// Containment/capture-only operation never runs automations.
    CaptureOnly,
}

impl AutomationMessageAcceptance {
    #[must_use]
    pub fn permits_automations(self) -> bool {
        matches!(self, Self::AutomodDisabled | Self::Unmatched | Self::Exempt)
    }
}

/// A lookup key from an automod-accepted guild message. The adapter must call
/// this only after guild/channel scoping and moderation acceptance. Both flags
/// gate content processing, not just the MessageContent intent.
#[must_use]
pub fn accepted_text_trigger(
    automations_enabled: bool,
    text_commands_enabled: bool,
    author_is_bot: bool,
    content: &str,
    builtins: &HashSet<String>,
) -> Option<String> {
    if !automations_enabled || !text_commands_enabled || author_is_bot {
        return None;
    }
    trigger_word(content).filter(|word| !is_builtin_trigger(word, builtins))
}

/// Actionable, input-independent refusal for malformed or unavailable pages.
pub const INVALID_COMMAND_LIST_PAGE: &str =
    "Invalid page. Run /command-list page:1, then use the next-page hint.";

/// Legacy callers select the first whole-row page.
#[must_use]
pub fn format_command_list(rows: &[StoredCommand]) -> String {
    format_command_list_page(rows, 1).unwrap_or_else(str::to_owned)
}

/// Pack the existing bounded guild snapshot in its stable store order. Reserve
/// footer/navigation space using the row count: there cannot be more pages than
/// rows, so every page number fits without changing page boundaries mid-render.
/// No row is truncated, including defensive non-ASCII renderer inputs.
pub fn format_command_list_page(
    rows: &[StoredCommand],
    page: usize,
) -> Result<String, &'static str> {
    if page == 0 {
        return Err(INVALID_COMMAND_LIST_PAGE);
    }
    if rows.is_empty() {
        return if page == 1 {
            Ok("No custom commands defined.".to_owned())
        } else {
            Err(INVALID_COMMAND_LIST_PAGE)
        };
    }
    let lines: Vec<_> = rows
        .iter()
        .map(|row| {
            let trigger = row
                .text_trigger
                .as_deref()
                .map(|trigger| format!(" (or {})", escape_inspect_text(trigger)))
                .unwrap_or_default();
            format!(
                "/{}{} — {}",
                escape_inspect_text(&row.name),
                trigger,
                if row.enabled { "on" } else { "off" }
            )
        })
        .collect();
    // A single page needs no next hint. Do not reject a complete snapshot
    // that fits exactly with its shorter footer.
    let single_page = format!("{}\n\nPage 1 of 1", lines.join("\n"));
    if single_page.encode_utf16().count() <= MAX_RENDERED_CHARS {
        return if page == 1 {
            Ok(single_page)
        } else {
            Err(INVALID_COMMAND_LIST_PAGE)
        };
    }
    let reserved = format!(
        "\n\nPage {0} of {0}\nNext: /command-list page:{0}",
        rows.len()
    )
    .encode_utf16()
    .count();
    let body_limit = MAX_RENDERED_CHARS.saturating_sub(reserved);
    let mut pages = Vec::new();
    let mut body = String::new();
    let mut units = 0;
    for line in lines {
        let line_units = line.encode_utf16().count();
        if line_units > body_limit {
            return Err(
                "A custom-command row is too long to list. Ask a server admin to repair it, then run /command-list page:1.",
            );
        }
        if units + usize::from(!body.is_empty()) + line_units > body_limit {
            pages.push(std::mem::take(&mut body));
            units = 0;
        }
        if !body.is_empty() {
            body.push('\n');
            units += 1;
        }
        body.push_str(&line);
        units += line_units;
    }
    pages.push(body);
    let total = pages.len();
    let body = pages.get(page - 1).ok_or(INVALID_COMMAND_LIST_PAGE)?;
    let mut reply = format!("{body}\n\nPage {page} of {total}");
    if page < total {
        reply.push_str(&format!("\nNext: /command-list page:{}", page + 1));
    }
    Ok(reply)
}

/// `/command-inspect` sample-arguments bound in UTF-16 units, matching the
/// published `max_length`. Samples are validated for size only: the
/// four-variable template language ignores them, so accepting a sample must
/// never introduce `{args}` or a second interpolation pass.
pub const MAX_INSPECT_ARGUMENT_UNITS: usize = 500;

/// Which private view `/command-inspect` renders. Summary never bundles the
/// template body or a preview: two full 2,000-unit bodies plus metadata
/// cannot fit one content-only response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectView {
    Summary,
    Template,
    Preview,
}

impl InspectView {
    /// Parse the `view` option. Anything else (including `execute`) refuses
    /// with a correction example; the Discord layer defaults to summary.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "summary" => Some(Self::Summary),
            "template" => Some(Self::Template),
            "preview" => Some(Self::Preview),
            _ => None,
        }
    }

    /// Trusted plain-text heading, always the first line, always followed by
    /// a newline before untrusted text. The validated name still renders
    /// through [`escape_inspect_text`]: a stored `__name__` is active
    /// Markdown until escaped.
    #[must_use]
    pub fn heading(self, name: &str) -> String {
        let view = match self {
            Self::Summary => "Summary",
            Self::Template => "Saved template",
            Self::Preview => "Preview",
        };
        format!("{view} /{}", escape_inspect_text(name))
    }
}

/// Escape untrusted saved/description/rendered text for an inspect view body:
/// fold line breaks (a view stays one paragraph after its trusted heading)
/// and backslash-escape Markdown metacharacters. Mentions, pills, links and
/// braces pass through literally — mention safety comes from the reply's
/// explicit empty `allowed_mentions`, never from text mutation — so an admin
/// sees the saved text as stored. Plain text passes through byte-identical.
#[must_use]
pub fn escape_inspect_text(value: &str) -> String {
    // Folding newlines leaves only the initial non-whitespace prefix able to
    // open a block. Escape its marker without changing mention/channel pills
    // or punctuation elsewhere in the body.
    let leading = value.trim_start_matches(char::is_whitespace);
    let digits = leading.bytes().take_while(u8::is_ascii_digit).count();
    let block_marker = if leading.starts_with(['#', '>', '-', '+']) {
        Some(value.len() - leading.len())
    } else if digits > 0 && leading[digits..].starts_with(['.', ')']) {
        Some(value.len() - leading.len() + digits)
    } else {
        None
    };
    let mut escaped = String::with_capacity(value.len());
    for (index, ch) in value.char_indices() {
        if matches!(ch, '\r' | '\n') {
            escaped.push(' ');
        } else {
            if Some(index) == block_marker
                || matches!(ch, '\\' | '`' | '*' | '_' | '~' | '|' | '[' | ']')
            {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

/// Longest prefix within `limit` UTF-16 units, cut on an escape-pair
/// boundary so a trailing lone backslash cannot eat the truncation marker.
/// Only ever cuts escaped text, where every backslash starts a pair.
fn truncate_escaped_units(value: &str, limit: usize) -> String {
    let mut units = 0;
    let mut end = 0;
    for (index, ch) in value.char_indices() {
        let width = ch.len_utf16();
        if units + width > limit {
            break;
        }
        units += width;
        end = index + ch.len_utf8();
    }
    let mut prefix = value[..end].to_owned();
    if prefix.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
        prefix.pop();
    }
    prefix
}

/// Normalize and validate a saved-command name for inspection: the same
/// shape rules as writes, but only dispatch-owned builtin names are excluded.
/// Write-only reservations must not hide grandfathered saved rows. Lowercases
/// first, like the management path.
pub fn validate_inspect_name(
    name: &str,
    builtins: &HashSet<String>,
) -> Result<String, CommandError> {
    let name = name.to_lowercase();
    if !valid_name(&name) {
        return Err(CommandError::InvalidName);
    }
    if builtins.contains(&name) {
        return Err(CommandError::ReservedName(name.clone()));
    }
    Ok(name)
}

/// Field-identifying validation refusal with a usable correction example.
/// Validation always precedes store access, so these never carry outage copy
/// and never look like a rendered preview.
#[must_use]
pub fn invalid_inspect_option_reply(field: &str, detail: &str) -> String {
    format!("Invalid {field}: {detail} Example: /command-inspect name:faq view:preview")
}

/// `view`-option refusal. A `None` raw value means a forged type or a
/// duplicate; otherwise the bounded escaped value is echoed for correction.
#[must_use]
pub fn invalid_inspect_view_reply(raw: Option<&str>) -> String {
    match raw {
        None => "Invalid view: expected a single text view. Expected one of: summary, template, preview. Example: /command-inspect name:faq view:preview".to_owned(),
        Some(raw) => {
            let shown = escape_inspect_text(raw);
            let shown = if shown.encode_utf16().count() > 64 {
                format!("{}…", truncate_escaped_units(&shown, 64))
            } else {
                shown
            };
            format!("Invalid view \"{shown}\": expected one of: summary, template, preview. Example: /command-inspect name:faq view:preview")
        }
    }
}

/// Truthful miss: unknown, other-guild-only and deleted definitions share
/// one reply that names the correction path, never foreign text or outage
/// copy.
#[must_use]
pub fn format_inspect_miss(name: &str) -> String {
    format!(
        "No command /{} found in this server. List saved commands with /command-list.",
        escape_inspect_text(name)
    )
}

/// Private definition summary: saved description, existing enabled state,
/// text trigger and usable template/preview/correction examples. Never the
/// template body or a preview — those are their own views. Disabled rows
/// stay inspectable; nothing here enables them.
#[must_use]
pub fn format_inspect_summary(row: &StoredCommand) -> String {
    let heading = InspectView::Summary.heading(&row.name);
    let status = if row.enabled { "Enabled" } else { "Disabled" };
    let trigger = escape_inspect_text(row.text_trigger.as_deref().unwrap_or("none (slash-only)"));
    let shown = escape_inspect_text(&row.name);
    let examples = format!(
        "\nSaved template: /command-inspect name:{shown} view:template\nPrivate preview: /command-inspect name:{shown} view:preview\nReplace the definition: /command name:{shown} template:..."
    );
    // Fixed overhead first: only the escaped description is unbounded (raw
    // rows predate validation), so only it can be cut — with an explicit
    // marker, never a silent partial.
    let fixed_units = heading.encode_utf16().count()
        + "\nDescription: ".encode_utf16().count()
        + "\nStatus: ".encode_utf16().count()
        + status.encode_utf16().count()
        + "\nText trigger: ".encode_utf16().count()
        + trigger.encode_utf16().count()
        + examples.encode_utf16().count();
    let mut description = escape_inspect_text(&row.description);
    let budget = MAX_RENDERED_CHARS.saturating_sub(fixed_units);
    if description.encode_utf16().count() > budget {
        const MARKER: &str = "…[truncated]";
        let cut = truncate_escaped_units(
            &description,
            budget.saturating_sub(MARKER.encode_utf16().count()),
        );
        description = format!("{cut}{MARKER}");
    }
    let reply = format!("{heading}\nDescription: {description}\nStatus: {status}\nText trigger: {trigger}{examples}");
    debug_assert!(reply.encode_utf16().count() <= MAX_RENDERED_CHARS);
    reply
}

/// Heading plus full escaped body when it fits; otherwise the heading, the
/// longest fitting escape-boundary prefix and an explicit truncation marker.
/// Total stays within Discord's content ceiling.
fn format_bounded_view(heading: &str, escaped: &str) -> String {
    let head_units = heading.encode_utf16().count() + 1;
    let total_units = escaped.encode_utf16().count();
    if head_units + total_units <= MAX_RENDERED_CHARS {
        return format!("{heading}\n{escaped}");
    }
    // Reserve marker space generously: the marker below never exceeds 60
    // units (fixed text plus two bounded counts), so the prefix plus the
    // exact marker always fits.
    let budget = MAX_RENDERED_CHARS
        .saturating_sub(head_units)
        .saturating_sub(60);
    let prefix = truncate_escaped_units(escaped, budget);
    let shown = prefix.encode_utf16().count();
    let reply = format!("{heading}\n{prefix}\n[truncated: showing {shown} of {total_units} units]");
    debug_assert!(reply.encode_utf16().count() <= MAX_RENDERED_CHARS);
    reply
}

/// Saved template as stored (escaped), or an explicit truncation marker —
/// never a misleading silent partial body. Shown regardless of validity: an
/// admin must see the broken template to fix it.
#[must_use]
pub fn format_inspect_template(row: &StoredCommand) -> String {
    format_bounded_view(
        &InspectView::Template.heading(&row.name),
        &escape_inspect_text(&row.template),
    )
}

/// Private rendered preview with the invoking member/channel context, or a
/// truthful refusal with a correction example. Invalid saved templates
/// refuse without rendering; the sample arguments are never interpolated.
#[must_use]
pub fn format_inspect_preview(row: &StoredCommand, ctx: &TemplateContext) -> String {
    if let Err(error) = validate_template(&row.template) {
        let shown = escape_inspect_text(&row.name);
        let prefix = format!("Template for /{shown} is invalid: ");
        let correction = format!(" Replace it with /command name:{shown} template:...");
        let mut detail = escape_inspect_text(&error.to_string());
        let budget = MAX_RENDERED_CHARS
            .saturating_sub(prefix.encode_utf16().count() + correction.encode_utf16().count());
        if detail.encode_utf16().count() > budget {
            const MARKER: &str = "…[truncated]";
            detail = format!(
                "{}{MARKER}",
                truncate_escaped_units(
                    &detail,
                    budget.saturating_sub(MARKER.encode_utf16().count())
                )
            );
        }
        return format!("{prefix}{detail}{correction}");
    }
    let rendered = match render_template(&row.template, ctx) {
        Ok(rendered) => rendered,
        Err(refusal @ TemplateError::RenderTooLong(_)) => {
            let shown = escape_inspect_text(&row.name);
            return format!(
                "Template for /{shown} renders too long to preview ({refusal}). Shorten it with /command name:{shown} template:..."
            );
        }
        Err(error) => {
            let shown = escape_inspect_text(&row.name);
            return format!(
                "Template for /{shown} cannot be previewed: {error} Replace it with /command name:{shown} template:..."
            );
        }
    };
    format_bounded_view(
        &InspectView::Preview.heading(&row.name),
        &escape_inspect_text(&rendered),
    )
}

/// Validate and preview proposed text without constructing or saving a command.
/// Uses the saved preview's single-pass renderer, escaping and framing; raw
/// expansion overflow refuses rather than presenting a partial successful render.
#[must_use]
pub fn format_draft_preview(template: &str, ctx: &TemplateContext) -> String {
    match validate_template(template).and_then(|()| render_template(template, ctx)) {
        Ok(rendered) => format_bounded_view(
            "Draft preview — not saved or published",
            &escape_inspect_text(&rendered),
        ),
        Err(error) => {
            let detail = escape_inspect_text(&error.to_string());
            let detail = if detail.encode_utf16().count() > 256 {
                format!("{}…[truncated]", truncate_escaped_units(&detail, 256))
            } else {
                detail
            };
            format!(
                "Draft cannot be previewed: {detail} Correct or shorten the draft and retry /command-inspect draft:Hello. Nothing was saved or published."
            )
        }
    }
}

/// Preview refused for missing render context (no invoking member, channel
/// or guild name): truthful, with no template-correction wording, since the
/// saved definition is not at fault.
#[must_use]
pub fn inspect_preview_context_unavailable(name: &str) -> String {
    format!(
        "Preview for /{} is unavailable: command context is missing. Please try again.",
        escape_inspect_text(name)
    )
}

/// DB-backed delete set for disable-time deregistration, sorted and deduped
/// (legacy `removeDbBackedCommands`: scoped from the DATABASE — disabled rows
/// included, since a row disabled *after* publication still has a live
/// command — never from Discord's live list, which also holds Owen builtins
/// and other applications' commands). The REST deletes themselves belong to
/// the executor slice (TOG-10076); this is the pure scoping rule.
#[must_use]
pub fn deregister_set(names: &[String]) -> Vec<String> {
    let mut set: Vec<String> = names.to_vec();
    set.sort();
    set.dedup();
    set
}

/// Merge dynamic rows into the published registry (parity #22): enabled rows
/// join after the builtins; builtin names shadow custom ones and the
/// 100-command ceiling is enforced (both in [`merge_commands`]).
pub fn registry_with_custom(
    additional_builtins: &[Vec<CommandDefinition>],
    rows: &[StoredCommand],
) -> Result<Vec<CommandDefinition>, super::commands::RegistryError> {
    let custom: Vec<CustomCommand> = rows.iter().map(StoredCommand::registry_entry).collect();
    merge_commands(additional_builtins, &custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: &str = "1545644954272137297";
    const ACTOR: &str = "900000000000000001";

    fn stored(name: &str) -> StoredCommand {
        StoredCommand {
            guild_id: GUILD.to_owned(),
            name: name.to_owned(),
            description: format!("The {name} command"),
            template: format!("Hello {{user}}, welcome to {{server}} via {name}!"),
            text_trigger: None,
            enabled: true,
        }
    }

    fn put_input(name: &str) -> PutCommandInput {
        PutCommandInput {
            name: name.to_owned(),
            description: "FAQ answer".to_owned(),
            template: "See {channel}, {username}!".to_owned(),
            text_trigger: Some("!faq".to_owned()),
        }
    }

    #[test]
    fn automation_acceptance_requires_a_known_nonmatching_decision() {
        use AutomationMessageAcceptance::*;
        for (decision, allowed) in [
            (AutomodDisabled, true),
            (Unmatched, true),
            (Exempt, true),
            (Matched, false),
            (Unavailable, false),
            (CaptureOnly, false),
        ] {
            assert_eq!(decision.permits_automations(), allowed, "{decision:?}");
        }
    }

    // --- template -----------------------------------------------------------

    #[test]
    fn template_known_placeholders_validate() {
        validate_template("Hello {user} in {server}/{channel} aka {username}").expect("valid");
    }

    #[test]
    fn template_unknown_placeholder_is_definition_time_error() {
        assert_eq!(
            validate_template("Hi {usre}"),
            Err(TemplateError::UnknownPlaceholder("usre".to_owned()))
        );
    }

    #[test]
    fn template_uppercase_braces_are_literal_not_placeholders() {
        // Legacy `/\{([a-z]+)\}/g` never matches these either.
        validate_template("Hi {User} {USER} {} {user-name}").expect("all literal");
        assert_eq!(placeholders_in("{User} {}"), Vec::<String>::new());
    }

    #[test]
    fn template_placeholders_in_dedupes_in_order() {
        assert_eq!(
            placeholders_in("{user} and {user} and {server}"),
            ["user".to_owned(), "server".to_owned()]
        );
    }

    #[test]
    fn template_placeholders_in_is_total_bounded_on_hostile_input() {
        // 200 distinct lowercase keys: unbounded extraction would return all
        // 200; the hold caps distinct placeholders so fuzz-sized inputs stay
        // bounded in time and memory.
        fn alpha_key(mut n: usize) -> String {
            let mut key = Vec::new();
            loop {
                key.push((b'a' + (n % 26) as u8) as char);
                n /= 26;
                if n == 0 {
                    break;
                }
                n -= 1;
            }
            key.into_iter().rev().collect()
        }
        let hostile: String = (0..200).map(|i| format!("{{{}}}", alpha_key(i))).collect();
        let found = placeholders_in(&hostile);
        assert!(
            found.len() <= 64,
            "placeholders_in must stay total-bounded, got {}",
            found.len()
        );
        // First-seen order preserved for the capped prefix.
        assert_eq!(found[0], alpha_key(0));
        assert_eq!(found[1], alpha_key(1));
    }

    #[test]
    fn template_render_hostile_expansion_refused_without_panic() {
        // Degenerate expansion: many placeholders over a large context must
        // fail as a bounded RenderTooLong, never panic, hang, or blow up to
        // the full expansion. The byte tripwire refuses once buffered bytes
        // breach it (here the second push sees 10_000 buffered bytes),
        // reporting bytes always past the ceiling.
        let ctx = TemplateContext {
            user: "y".repeat(10_000),
            username: String::new(),
            server: String::new(),
            channel: String::new(),
        };
        let template = "{user}".repeat(500);
        let result = render_template(&template, &ctx);
        assert!(
            matches!(
                result,
                Err(TemplateError::RenderTooLong(len)) if len > MAX_RENDERED_CHARS
            ),
            "expected bounded RenderTooLong, got {result:?}"
        );
    }

    #[test]
    fn template_lengths_bounded() {
        assert_eq!(validate_template(""), Err(TemplateError::InvalidLength));
        assert_eq!(
            validate_template(&"x".repeat(2001)),
            Err(TemplateError::InvalidLength)
        );
        validate_template(&"x".repeat(2000)).expect("ceiling ok");
    }

    #[test]
    fn template_render_substitutes_all_four() {
        let ctx = TemplateContext {
            user: "<@1>".to_owned(),
            username: "one".to_owned(),
            server: "TWO".to_owned(),
            channel: "#general".to_owned(),
        };
        assert_eq!(
            render_template("hey {user} ({username}) of {server} in {channel}", &ctx),
            Ok("hey <@1> (one) of TWO in #general".to_owned())
        );
    }

    #[test]
    fn template_render_over_ceiling_refused() {
        let ctx = TemplateContext {
            user: String::new(),
            username: "x".repeat(50),
            server: String::new(),
            channel: String::new(),
        };
        assert!(matches!(
            render_template(&format!("{{username}}{}", "x".repeat(2000)), &ctx),
            Err(TemplateError::RenderTooLong(_))
        ));
    }

    #[test]
    fn template_lengths_match_legacy_utf16_including_emoji() {
        validate_template(&"😀".repeat(1000)).expect("2000 UTF-16 units");
        assert_eq!(
            validate_template(&"😀".repeat(1001)),
            Err(TemplateError::InvalidLength)
        );
        let ctx = TemplateContext {
            user: String::new(),
            username: "😀".repeat(1001),
            server: String::new(),
            channel: String::new(),
        };
        assert_eq!(
            render_template("{username}", &ctx),
            Err(TemplateError::RenderTooLong(2002))
        );
        let mut input = put_input("faq");
        input.description = "😀".repeat(51);
        assert_eq!(
            validate_put_input(&input, &HashSet::new()),
            Err(CommandError::InvalidDescription)
        );
    }

    #[test]
    fn substitution_is_single_pass_and_preserves_unicode_literals() {
        let ctx = TemplateContext {
            user: "{username}".to_owned(),
            username: "not substituted twice".to_owned(),
            server: "世界".to_owned(),
            channel: "#rules".to_owned(),
        };
        assert_eq!(
            render_template("😀 {user} {{server}} {User} {unknown}", &ctx),
            Ok("😀 {username} {世界} {User} {unknown}".to_owned())
        );
    }

    // --- validation ---------------------------------------------------------

    #[test]
    fn put_validation_accepts_well_formed() {
        validate_put_input(&put_input("faq"), &builtin_command_names()).expect("valid");
    }

    #[test]
    fn put_validation_rejects_bad_names() {
        let builtins = builtin_command_names();
        for bad in [
            "",
            "FAQ",
            "has space",
            "bang!",
            "a".repeat(33).as_str(),
            "semi;colon",
        ] {
            let mut input = put_input(bad);
            // Trigger must stay valid so the name is what fails.
            input.text_trigger = None;
            assert_eq!(
                validate_put_input(&input, &builtins),
                Err(CommandError::InvalidName),
                "{bad:?} refused"
            );
        }
    }

    #[test]
    fn put_validation_refuses_builtin_collision() {
        let builtins = builtin_command_names();
        // Moderation, leveling, and the builder commands themselves reserve.
        for reserved in [
            "ban",
            "rank",
            "command",
            "command-remove",
            "schedule",
            "rsvp",
        ] {
            let mut input = put_input(reserved);
            input.text_trigger = None;
            assert_eq!(
                validate_put_input(&input, &builtins),
                Err(CommandError::ReservedName(reserved.to_owned())),
                "{reserved} reserved"
            );
        }
    }

    #[test]
    fn dispatch_reservation_stays_free_of_voice_names() {
        // Only writes reserve voice names. A stored row under one keeps running
        // while voice is off, so dispatch must not treat the name as builtin.
        let dispatch = builtin_command_names();
        let reserved = reserved_command_names();
        let voice = crate::voice_rooms::voice_commands();
        assert!(dispatch.is_subset(&reserved));
        assert!(!dispatch.contains("setup"));
        assert!(voice.iter().all(|def| reserved.contains(&def.name)));
    }

    #[test]
    fn put_validation_refuses_every_voice_room_name() {
        // The voice sink matches by name, so a custom command named after a
        // voice command would reach the voice handlers.
        let builtins = reserved_command_names();
        let voice = crate::voice_rooms::voice_commands();
        assert!(voice.iter().any(|def| def.name == "setup"));
        for def in &voice {
            let mut input = put_input(&def.name);
            input.text_trigger = None;
            assert_eq!(
                validate_put_input(&input, &builtins),
                Err(CommandError::ReservedName(def.name.clone())),
                "{} reserved",
                def.name
            );
        }
    }

    #[test]
    fn write_reservations_cover_the_maximal_publish_set_and_capacity() {
        use crate::commands::CustomCommand;
        use crate::router::{InteractionRouter, RouterGates};

        let router = InteractionRouter::new(RouterGates {
            scorecard: true,
            automations: true,
            announcements: true,
            moderation: true,
            voice: true,
            voice_assistant: true,
            configured_guild: Some(1),
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        });
        let reserved = reserved_command_names();
        let definitions = router.publish_set(&[]).unwrap();
        let published: HashSet<_> = definitions.iter().map(|def| def.name.clone()).collect();
        // `templateassistant` is unpublished until its slice lands, but the name
        // stays squatted: reservations cover the publish set plus exactly that
        // one held name (which keeps holding its guild slot in the capacity
        // math below, so the slot count does not churn when it republishes).
        assert!(published.is_subset(&reserved));
        assert_eq!(reserved.len(), published.len() + 1);
        assert!(reserved.contains("templateassistant"));
        assert!(!published.contains("templateassistant"));
        assert_eq!(
            validate_put_input(&put_input("templateassistant"), &reserved),
            Err(CommandError::ReservedName("templateassistant".to_owned()))
        );
        let capacity = max_custom_commands(reserved.len());
        let custom: Vec<_> = (0..capacity)
            .map(|index| CustomCommand {
                name: format!("custom-{index}"),
                description: "Custom command".to_owned(),
                enabled: true,
            })
            .collect();
        assert_eq!(
            router.publish_set(&custom).unwrap().len(),
            published.len() + capacity
        );
        assert_eq!(
            check_capacity(capacity, true, reserved.len()),
            Err(CommandError::OverCapacity(capacity))
        );
    }

    #[test]
    fn put_validation_bounds_description_and_template() {
        let builtins = HashSet::new();
        let mut input = put_input("faq");
        input.description = String::new();
        assert_eq!(
            validate_put_input(&input, &builtins),
            Err(CommandError::InvalidDescription)
        );
        input.description = "x".repeat(101);
        assert_eq!(
            validate_put_input(&input, &builtins),
            Err(CommandError::InvalidDescription)
        );
        input.description = "ok".to_owned();
        input.template = "Hi {usre}".to_owned();
        assert_eq!(
            validate_put_input(&input, &builtins),
            Err(CommandError::InvalidTemplate(
                TemplateError::UnknownPlaceholder("usre".to_owned())
            ))
        );
    }

    #[test]
    fn put_validation_bounds_trigger_shape() {
        let builtins = HashSet::new();
        for bad in [
            "faq".to_owned(),
            "!".to_owned(),
            "!FAQ".to_owned(),
            "!has space".to_owned(),
            format!("!{}", "a".repeat(33)),
        ] {
            let mut input = put_input("faq");
            input.text_trigger = Some(bad.clone());
            assert_eq!(
                validate_put_input(&input, &builtins),
                Err(CommandError::InvalidTextTrigger),
                "{bad:?} refused"
            );
        }
    }

    #[test]
    fn capacity_permits_updates_but_refuses_new_names_at_ceiling() {
        // 27 ported builtins reserve; 73 custom fit.
        assert_eq!(max_custom_commands(27), 73);
        assert!(check_capacity(73, false, 27).is_ok());
        assert_eq!(
            check_capacity(73, true, 27),
            Err(CommandError::OverCapacity(73))
        );
        assert!(check_capacity(72, true, 27).is_ok());
    }

    // --- gates ---------------------------------------------------------------

    #[test]
    fn disabled_automations_refuse_admin_and_dynamic() {
        assert_eq!(
            require_automations_enabled(false),
            Err(CommandError::AutomationsDisabled)
        );
        assert_eq!(
            adjudicate_run(false, Some(&stored("faq"))),
            RunOutcome::RefusedDisabled
        );
        // Refusal message is the post-425 actionable reply text.
        assert_eq!(
            AUTOMATIONS_DISABLED_REPLY,
            "Automations are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role."
        );
    }

    #[test]
    fn run_renders_enabled_row_and_ignores_rest() {
        assert_eq!(
            adjudicate_run(true, Some(&stored("faq"))),
            RunOutcome::Render {
                name: "faq".to_owned(),
                template: "Hello {user}, welcome to {server} via faq!".to_owned(),
            }
        );
        // Unknown name: silent.
        assert_eq!(adjudicate_run(true, None), RunOutcome::Ignored);
        // Disabled row: silent (it should never have been published).
        let mut row = stored("faq");
        row.enabled = false;
        assert_eq!(adjudicate_run(true, Some(&row)), RunOutcome::Ignored);
    }

    // --- triggers ------------------------------------------------------------

    #[test]
    fn trigger_word_reads_first_token_lowercased() {
        assert_eq!(trigger_word("!faq"), Some("!faq".to_owned()));
        assert_eq!(trigger_word("!FAQ please"), Some("!faq".to_owned()));
        assert_eq!(trigger_word("!faq!bar"), Some("!faq!bar".to_owned()));
        assert_eq!(trigger_word("!"), None);
        assert_eq!(trigger_word("hi !faq"), None);
        assert_eq!(trigger_word(""), None);
    }

    #[test]
    fn builtin_names_excluded_from_triggers() {
        let builtins = builtin_command_names();
        assert!(is_builtin_trigger("!ban", &builtins));
        assert!(is_builtin_trigger("!rank", &builtins));
        assert!(!is_builtin_trigger("!faq", &builtins));
    }

    #[test]
    fn accepted_text_triggers_require_both_gates_and_human_author() {
        let builtins = builtin_command_names();
        for automations in [false, true] {
            for text in [false, true] {
                for bot in [false, true] {
                    assert_eq!(
                        accepted_text_trigger(automations, text, bot, "!FAQ ignored", &builtins),
                        if automations && text && !bot {
                            Some("!faq".to_owned())
                        } else {
                            None
                        }
                    );
                }
            }
        }
        for content in ["!BAN reason", "hello !faq", "! faq", "", "!\nfaq"] {
            assert_eq!(
                accepted_text_trigger(true, true, false, content, &builtins),
                None
            );
        }
    }

    // --- decisions -----------------------------------------------------------

    #[test]
    fn put_decision_marks_created_and_resyncs() {
        let d = adjudicate_put(GUILD, ACTOR, "faq", None);
        assert!(d.created && d.resync_registry);
        assert_eq!(
            d.audit,
            AuditRecord {
                guild_id: GUILD.to_owned(),
                actor_id: Some(ACTOR.to_owned()),
                action: "command.create".to_owned(),
                target_key: Some("faq".to_owned()),
                outcome: "ok".to_owned(),
                reason: None,
            }
        );
        let d = adjudicate_put(GUILD, ACTOR, "faq", Some(&stored("faq")));
        assert!(!d.created && d.resync_registry);
        assert_eq!(d.audit.action, "command.update");
    }

    #[test]
    fn put_rejection_audits_with_stable_code() {
        let err = CommandError::ReservedName("ban".to_owned());
        let audit = AuditRecord::put_rejected(GUILD, ACTOR, "ban", false, &err);
        assert_eq!(audit.action, "command.create");
        assert_eq!(audit.outcome, "rejected");
        assert_eq!(audit.reason.as_deref(), Some("reserved_name"));
        let audit = AuditRecord::put_rejected(GUILD, ACTOR, "faq", true, &err);
        assert_eq!(audit.action, "command.update");
    }

    #[test]
    fn delete_decision_resyncs_only_when_removed() {
        let d = adjudicate_delete(GUILD, ACTOR, "faq", true);
        assert!(d.deleted && d.resync_registry);
        assert_eq!(d.audit.outcome, "ok");
        let d = adjudicate_delete(GUILD, ACTOR, "faq", false);
        assert!(!d.deleted && !d.resync_registry);
        assert_eq!(d.audit.outcome, "absent");
    }

    #[test]
    fn toggle_decision_flips_audit_and_resync_and_nothing_else_does() {
        let mut paused = stored("faq");
        paused.enabled = false;
        for (row, enabled, outcome, resync) in [
            (None, true, ToggleOutcome::Missing, false),
            (None, false, ToggleOutcome::Missing, false),
            (Some(&paused), false, ToggleOutcome::Unchanged, false),
            (Some(&stored("faq")), true, ToggleOutcome::Unchanged, false),
            (Some(&paused), true, ToggleOutcome::Changed, true),
            (Some(&stored("faq")), false, ToggleOutcome::Changed, true),
        ] {
            let d = adjudicate_toggle(GUILD, ACTOR, "faq", row, enabled);
            assert_eq!(d.outcome, outcome, "enabled={enabled}");
            assert_eq!(d.enabled, enabled);
            assert_eq!(d.resync_registry, resync);
            assert_eq!(d.audit.is_some(), resync, "only flips audit");
        }
        let d = adjudicate_toggle(GUILD, ACTOR, "faq", Some(&paused), true);
        assert_eq!(
            d.audit.expect("changed toggle audits"),
            AuditRecord {
                guild_id: GUILD.to_owned(),
                actor_id: Some(ACTOR.to_owned()),
                action: "command.enable".to_owned(),
                target_key: Some("faq".to_owned()),
                outcome: "ok".to_owned(),
                reason: None,
            }
        );
        let d = adjudicate_toggle(GUILD, ACTOR, "faq", Some(&stored("faq")), false);
        let audit = d.audit.expect("changed toggle audits");
        assert_eq!(audit.action, "command.disable");
        assert_eq!(audit.outcome, "ok");
    }

    #[test]
    fn toggle_enable_revalidates_the_stored_definition() {
        let reserved = reserved_command_names();
        let dispatch = builtin_command_names();
        let mut valid = stored("faq");
        valid.text_trigger = Some("!faq".to_owned());
        valid.enabled = false;
        assert!(validate_toggle_enable(&valid, &reserved, &dispatch).is_ok());
        // A slash-only row has no trigger to collide.
        let mut slash_only = valid.clone();
        slash_only.text_trigger = None;
        assert!(validate_toggle_enable(&slash_only, &reserved, &dispatch).is_ok());
        // Reserved names stay reserved, like writes.
        for name in ["help", "command", "command-enable", "setup"] {
            let mut row = valid.clone();
            row.name = name.to_owned();
            assert_eq!(
                validate_toggle_enable(&row, &reserved, &dispatch),
                Err(CommandError::ReservedName(name.to_owned())),
                "{name} refused"
            );
        }
        // Invalid stored content refuses without reactivating.
        let mut broken = valid.clone();
        broken.template = "Unknown {args}".to_owned();
        assert!(matches!(
            validate_toggle_enable(&broken, &reserved, &dispatch),
            Err(CommandError::InvalidTemplate(_))
        ));
        // A builtin-named trigger can never fire, so it must not resume.
        for trigger in ["!rank", "!ban"] {
            let mut row = valid.clone();
            row.text_trigger = Some(trigger.to_owned());
            assert_eq!(
                validate_toggle_enable(&row, &reserved, &dispatch),
                Err(CommandError::ReservedTextTrigger(trigger.to_owned())),
                "{trigger} refused"
            );
        }
        // A grandfathered voice-named trigger still fires while voice is
        // off, so dispatch — not the write set — judges triggers.
        let mut voice_trigger = valid.clone();
        voice_trigger.text_trigger = Some("!setup".to_owned());
        assert!(validate_toggle_enable(&voice_trigger, &reserved, &dispatch).is_ok());
    }

    #[test]
    fn rejected_and_absent_audits_bound_untrusted_names() {
        let name = "😀".repeat(MAX_COMMAND_NAME_CHARS + 17);
        let expected = "😀".repeat(MAX_COMMAND_NAME_CHARS);
        let err = CommandError::ReservedName("ban".to_owned());

        let rejected = AuditRecord::put_rejected(GUILD, ACTOR, &name, false, &err);
        assert_eq!(rejected.outcome, "rejected");
        assert_eq!(rejected.target_key.as_deref(), Some(expected.as_str()));

        let absent = adjudicate_delete(GUILD, ACTOR, &name, false);
        assert_eq!(absent.audit.outcome, "absent");
        assert_eq!(absent.audit.target_key.as_deref(), Some(expected.as_str()));
    }

    fn successful_audits_expecting_target(
        name: &str,
        target: &str,
    ) -> [(&'static str, AuditRecord, AuditRecord); 7] {
        let expect = |action: &str, outcome: &str, reason: Option<&str>| AuditRecord {
            guild_id: GUILD.to_owned(),
            actor_id: Some(ACTOR.to_owned()),
            action: action.to_owned(),
            target_key: Some(target.to_owned()),
            outcome: outcome.to_owned(),
            reason: reason.map(str::to_owned),
        };
        [
            (
                "put created",
                adjudicate_put(GUILD, ACTOR, name, None).audit,
                expect("command.create", "ok", None),
            ),
            (
                "put updated",
                adjudicate_put(GUILD, ACTOR, name, Some(&stored("faq"))).audit,
                expect("command.update", "ok", None),
            ),
            (
                "delete ok",
                adjudicate_delete(GUILD, ACTOR, name, true).audit,
                expect("command.delete", "ok", None),
            ),
            (
                "run ok",
                AuditRecord::run(GUILD, ACTOR, name, true, None),
                expect("command.run", "ok", None),
            ),
            (
                "run failed",
                AuditRecord::run(GUILD, ACTOR, name, false, Some("delivery_failed")),
                expect("command.run", "failed", Some("delivery_failed")),
            ),
            (
                "toggle enable",
                AuditRecord::toggle(GUILD, ACTOR, name, true),
                expect("command.enable", "ok", None),
            ),
            (
                "toggle disable",
                AuditRecord::toggle(GUILD, ACTOR, name, false),
                expect("command.disable", "ok", None),
            ),
        ]
    }

    #[test]
    fn successful_audits_bound_target_key_by_characters_at_the_name_limit() {
        let bound = MAX_COMMAND_NAME_CHARS;
        for (input_chars, kept_chars) in [
            (bound - 1, bound - 1),
            (bound, bound),
            (bound + 1, bound),
            (bound + 17, bound),
        ] {
            let name = "😀".repeat(input_chars);
            let target = "😀".repeat(kept_chars);
            for (label, audit, expected) in successful_audits_expecting_target(&name, &target) {
                assert_eq!(audit, expected, "{label} with {input_chars} characters");
            }
        }
    }

    #[test]
    fn run_audit_marks_ok_and_failed() {
        let ok = AuditRecord::run(GUILD, ACTOR, "faq", true, None);
        assert_eq!(
            (ok.action.as_str(), ok.outcome.as_str()),
            ("command.run", "ok")
        );
        assert_eq!(ok.reason, None);
        let failed = AuditRecord::run(GUILD, ACTOR, "faq", false, Some("delivery_failed"));
        assert_eq!(failed.outcome, "failed");
        assert_eq!(failed.reason.as_deref(), Some("delivery_failed"));
    }

    // --- list / registry -----------------------------------------------------

    #[test]
    fn command_list_formats_lines_and_empty() {
        assert_eq!(format_command_list(&[]), "No custom commands defined.");
        let mut off = stored("rules");
        off.enabled = false;
        off.text_trigger = Some("!rules".to_owned());
        let rows = vec![stored("faq"), off];
        assert_eq!(
            format_command_list(&rows),
            "/faq — on\n/rules (or !rules) — off\n\nPage 1 of 1"
        );
    }

    #[test]
    fn dynamic_commands_join_registry_without_shadowing() {
        let feature = super::super::feature_commands::feature_commands();
        let moderation = super::super::moderation::moderation_commands();
        let rows = vec![stored("faq"), stored("ban")];
        let merged =
            registry_with_custom(&[feature, moderation], &rows).expect("merges under the ceiling");
        let names: Vec<_> = merged.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"faq"));
        // `ban` collides with the moderation builtin: first definition wins.
        assert_eq!(names.iter().filter(|n| ***n == *"ban").count(), 1);
        let ban = merged
            .iter()
            .find(|d| d.name == "ban")
            .expect("ban present");
        assert_eq!(
            ban.default_member_permissions,
            Some(super::super::commands::PERM_BAN_MEMBERS.to_string())
        );
    }

    #[test]
    fn deregister_set_scopes_from_db_sorted_and_deduped() {
        assert_eq!(
            deregister_set(&["b".to_owned(), "a".to_owned(), "b".to_owned()]),
            ["a".to_owned(), "b".to_owned()]
        );
        assert!(deregister_set(&[]).is_empty());
    }

    // --- inspect ------------------------------------------------------------

    fn inspect_row() -> StoredCommand {
        StoredCommand {
            guild_id: GUILD.to_owned(),
            name: "faq".to_owned(),
            description: "Saved FAQ description".to_owned(),
            template: "Hi {user} ({username}) in {server} {channel}".to_owned(),
            text_trigger: Some("!faq".to_owned()),
            enabled: true,
        }
    }

    fn inspect_ctx() -> TemplateContext {
        TemplateContext {
            user: "<@3333>".to_owned(),
            username: "tester".to_owned(),
            server: "TWO".to_owned(),
            channel: "<#4444>".to_owned(),
        }
    }

    #[test]
    fn inspect_view_parses_three_views_with_trusted_headings() {
        assert_eq!(InspectView::parse("summary"), Some(InspectView::Summary));
        assert_eq!(InspectView::parse("template"), Some(InspectView::Template));
        assert_eq!(InspectView::parse("preview"), Some(InspectView::Preview));
        assert_eq!(InspectView::parse("execute"), None);
        assert_eq!(InspectView::parse("Preview"), None);
        assert_eq!(InspectView::parse(""), None);
        assert_eq!(InspectView::Summary.heading("faq"), "Summary /faq");
        assert_eq!(InspectView::Template.heading("faq"), "Saved template /faq");
        assert_eq!(InspectView::Preview.heading("faq"), "Preview /faq");
    }

    #[test]
    fn inspect_escape_keeps_mentions_literal_and_breaks_markdown() {
        assert_eq!(escape_inspect_text("plain"), "plain");
        assert_eq!(
            escape_inspect_text("**FORGED** ``` @everyone <@&8888> [x](y) a|b_c~d"),
            "\\*\\*FORGED\\*\\* \\`\\`\\` @everyone <@&8888> \\[x\\](y) a\\|b\\_c\\~d"
        );
        assert_eq!(escape_inspect_text("a{b}c"), "a{b}c");
        assert_eq!(escape_inspect_text("one\ntwo\rthree"), "one two three");
        assert_eq!(escape_inspect_text("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn inspect_name_validation_matches_dispatch_rules_after_lowercasing() {
        let reserved = builtin_command_names();
        for name in ["setup", "export"] {
            assert_eq!(validate_inspect_name(name, &reserved), Ok(name.to_owned()));
        }
        assert_eq!(
            validate_inspect_name("FAQ", &reserved).expect("normalizes"),
            "faq"
        );
        for bad in [
            "",
            " bad",
            "bad name",
            "f/aq",
            "@everyone",
            "../faq",
            "世界",
        ] {
            assert_eq!(
                validate_inspect_name(bad, &reserved),
                Err(CommandError::InvalidName),
                "{bad:?}"
            );
        }
        assert_eq!(
            validate_inspect_name(&"a".repeat(33), &reserved),
            Err(CommandError::InvalidName)
        );
        assert_eq!(
            validate_inspect_name("command-inspect", &reserved),
            Err(CommandError::ReservedName("command-inspect".to_owned()))
        );
    }

    #[test]
    fn inspect_summary_bundles_metadata_examples_and_no_template_body() {
        let summary = format_inspect_summary(&inspect_row());
        let (heading, body) = summary.split_once('\n').expect("heading line");
        assert_eq!(heading, "Summary /faq");
        assert!(body.contains("Saved FAQ description"));
        assert!(body.contains("Enabled"));
        assert!(body.contains("!faq"));
        assert!(body.contains("/command-inspect name:faq view:template"));
        assert!(body.contains("/command-inspect name:faq view:preview"));
        assert!(body.contains("/command name:faq template:"));
        assert!(!summary.contains("Hi {user}"));
        assert!(summary.encode_utf16().count() <= MAX_RENDERED_CHARS);
    }

    #[test]
    fn inspect_summary_marks_disabled_rows_without_enabling_text() {
        let mut row = inspect_row();
        row.enabled = false;
        row.text_trigger = None;
        let summary = format_inspect_summary(&row);
        assert!(summary.contains("Disabled"));
        assert!(!summary.to_lowercase().contains("enabled"));
        assert!(summary.contains("none (slash-only)"));
    }

    #[test]
    fn inspect_template_shows_saved_text_or_an_explicit_marker() {
        let view = format_inspect_template(&inspect_row());
        assert_eq!(
            view,
            "Saved template /faq\nHi {user} ({username}) in {server} {channel}"
        );
        let mut row = inspect_row();
        row.template = "x".repeat(2000);
        let view = format_inspect_template(&row);
        assert!(view.starts_with("Saved template /faq\n"));
        assert!(view.to_lowercase().contains("truncated"));
        assert!(view.encode_utf16().count() <= MAX_RENDERED_CHARS);
        // Escaped-output expansion counts toward the bound, not the raw body.
        row.template = "*".repeat(1990);
        let view = format_inspect_template(&row);
        assert!(view.to_lowercase().contains("truncated"));
        assert!(view.encode_utf16().count() <= MAX_RENDERED_CHARS);
        assert_eq!(
            view,
            format!(
                "Saved template /faq\n{}\n[truncated: showing 1920 of 3980 units]",
                "\\*".repeat(960)
            )
        );
    }

    #[test]
    fn inspect_template_and_preview_escape_block_markdown_after_leading_whitespace() {
        for (raw, escaped) in [
            ("# Welcome", "\\# Welcome"),
            ("  ## Welcome", "  \\## Welcome"),
            ("-# small", "\\-# small"),
            ("> Approved", "\\> Approved"),
            ("\t>>> Approved", "\t\\>>> Approved"),
            ("- item", "\\- item"),
            ("  + item", "  \\+ item"),
            ("1. item", "1\\. item"),
            ("  123) item", "  123\\) item"),
            ("\n # Welcome", "  \\# Welcome"),
            ("plain - text # tag 1.0", "plain - text # tag 1.0"),
            ("<@3333> <#4444> @everyone", "<@3333> <#4444> @everyone"),
        ] {
            let row = StoredCommand {
                template: raw.to_owned(),
                ..inspect_row()
            };
            assert_eq!(
                format_inspect_template(&row),
                format!("Saved template /faq\n{escaped}"),
                "saved {raw:?}"
            );
            let row = StoredCommand {
                template: "{username}".to_owned(),
                ..row
            };
            let ctx = TemplateContext {
                username: raw.to_owned(),
                ..inspect_ctx()
            };
            assert_eq!(
                format_inspect_preview(&row, &ctx),
                format!("Preview /faq\n{escaped}"),
                "rendered {raw:?}"
            );
        }
    }

    #[test]
    fn inspect_truncation_ends_on_a_complete_escape_pair_with_an_odd_budget() {
        let row = StoredCommand {
            name: "faqs".to_owned(),
            template: "*".repeat(1990),
            ..inspect_row()
        };
        assert_eq!(
            format_inspect_template(&row),
            format!(
                "Saved template /faqs\n{}\n[truncated: showing 1918 of 3980 units]",
                "\\*".repeat(959)
            )
        );
    }

    #[test]
    fn inspect_long_invalid_template_preserves_bounded_detail_and_correction() {
        let row = StoredCommand {
            template: format!("{{{}}}", "a".repeat(1998)),
            ..inspect_row()
        };
        let view = format_inspect_preview(&row, &inspect_ctx());
        assert!(view.encode_utf16().count() <= MAX_RENDERED_CHARS);
        assert!(view.starts_with("Template for /faq is invalid: Unknown placeholder"));
        assert!(view.contains("…[truncated]"));
        assert!(view.ends_with(" Replace it with /command name:faq template:..."));
    }

    #[test]
    fn draft_long_invalid_template_preserves_bounded_detail_and_correction() {
        let template = format!("{{{}}}", "a".repeat(1998));
        let view = format_draft_preview(&template, &inspect_ctx());
        assert!(view.encode_utf16().count() <= MAX_RENDERED_CHARS);
        assert!(view.starts_with("Draft cannot be previewed: Unknown placeholder"));
        assert!(view.contains("…[truncated]"));
        assert!(view.contains("retry /command-inspect draft:Hello."));
        assert!(view.ends_with("Nothing was saved or published."));
    }

    #[test]
    fn inspect_preview_renders_exactly_then_escapes() {
        let view = format_inspect_preview(&inspect_row(), &inspect_ctx());
        assert_eq!(view, "Preview /faq\nHi <@3333> (tester) in TWO <#4444>");
        let mut row = inspect_row();
        row.template = "{user}|{username}|{server}|{channel}|{{user}}".to_owned();
        let view = format_inspect_preview(&row, &inspect_ctx());
        assert_eq!(
            view,
            "Preview /faq\n<@3333>\\|tester\\|TWO\\|<#4444>\\|{<@3333>}"
        );
    }

    #[test]
    fn inspect_preview_refuses_invalid_templates_with_correction() {
        for template in ["Hello {unknown}", "{args}", &"😀".repeat(1001)] {
            let mut row = inspect_row();
            row.template = template.to_owned();
            let view = format_inspect_preview(&row, &inspect_ctx());
            assert!(view.to_lowercase().contains("template"), "{view}");
            assert!(!view.starts_with("Preview /faq\n"));
            assert!(view.contains("/command name:faq template:"), "{view}");
            assert!(!view.contains("temporarily unavailable"));
        }
        // Rendered overflow names the ceiling instead of a misleading body.
        let mut row = inspect_row();
        row.template = "{username}".repeat(200);
        let ctx = TemplateContext {
            username: "😀".repeat(32),
            ..inspect_ctx()
        };
        let view = format_inspect_preview(&row, &ctx);
        assert!(view.to_lowercase().contains("too long"), "{view}");
        assert!(view.contains("/command name:faq template:"), "{view}");
        assert!(!view.contains(&"😀".repeat(6400)));
    }

    #[test]
    fn inspect_miss_and_validation_replies_identify_the_correction_path() {
        let miss = format_inspect_miss("faq");
        assert!(miss.contains("No command /faq"));
        assert!(miss.contains("/command-list"));
        assert!(!miss.contains("temporarily unavailable"));
        for (field, reply) in [
            (
                "name",
                invalid_inspect_option_reply("command name", "a saved command name is required."),
            ),
            ("view", invalid_inspect_view_reply(Some("execute"))),
            ("view", invalid_inspect_view_reply(None)),
            (
                "arguments",
                invalid_inspect_option_reply(
                    "arguments",
                    "sample arguments must be at most 500 characters.",
                ),
            ),
        ] {
            assert!(reply.to_lowercase().contains(field), "{reply}");
            assert!(reply.contains("/command-inspect"), "{reply}");
            assert!(!reply.contains("temporarily unavailable"));
            assert!(!reply.contains("Preview /faq\n"));
        }
        assert!(invalid_inspect_view_reply(Some("execute")).contains("\"execute\""));
        assert!(
            invalid_inspect_view_reply(Some(&"v".repeat(200))).contains("…"),
            "unbounded forgery is cut with a marker"
        );
        assert!(
            inspect_preview_context_unavailable("faq").contains("Preview for /faq is unavailable")
        );
    }

    /// A valid underscore name is Markdown-active until escaped. Each inspect
    /// projection must show it literally; ordinary names stay byte-identical.
    #[test]
    fn inspect_projections_render_underscore_names_literally() {
        for view in [
            InspectView::Summary,
            InspectView::Template,
            InspectView::Preview,
        ] {
            let plain = view.heading("faq");
            let hostile = view.heading("__forged__");
            assert_eq!(hostile, plain.replace("faq", "\\_\\_forged\\_\\_"));
        }
        assert_eq!(
            format_inspect_miss("__forged__"),
            "No command /\\_\\_forged\\_\\_ found in this server. List saved commands with /command-list."
        );
        assert_eq!(
            format_inspect_miss("faq"),
            "No command /faq found in this server. List saved commands with /command-list."
        );
        assert_eq!(
            inspect_preview_context_unavailable("__forged__"),
            "Preview for /\\_\\_forged\\_\\_ is unavailable: command context is missing. Please try again."
        );
        assert_eq!(
            inspect_preview_context_unavailable("faq"),
            "Preview for /faq is unavailable: command context is missing. Please try again."
        );
    }

    #[test]
    fn inspect_invalid_template_refusal_escapes_underscore_names() {
        let row = StoredCommand {
            name: "__forged__".to_owned(),
            template: "{nope}".to_owned(),
            ..inspect_row()
        };
        let view = format_inspect_preview(&row, &inspect_ctx());
        assert!(
            view.starts_with("Template for /\\_\\_forged\\_\\_ is invalid: Unknown placeholder"),
            "{view}"
        );
        assert!(
            view.ends_with(" Replace it with /command name:\\_\\_forged\\_\\_ template:..."),
            "{view}"
        );
        assert!(!view.contains("__forged__"), "{view}");
    }

    #[test]
    fn inspect_summary_overflow_and_list_escape_underscore_names() {
        let mut row = inspect_row();
        row.name = "__forged__".to_owned();
        row.text_trigger = Some("!__forged__".to_owned());
        let summary = format_inspect_summary(&row);
        assert!(
            summary.contains(
                "Saved template: /command-inspect name:\\_\\_forged\\_\\_ view:template\n\
             Private preview: /command-inspect name:\\_\\_forged\\_\\_ view:preview\n\
             Replace the definition: /command name:\\_\\_forged\\_\\_ template:..."
            ),
            "{summary}"
        );
        assert!(!summary.contains("name:__forged__"), "{summary}");

        // 200 repetitions of a 32-unit username overflow the 2000-unit render cap.
        row.template = "{username}".repeat(200);
        let ctx = TemplateContext {
            username: "u".repeat(32),
            ..inspect_ctx()
        };
        let refusal = format_inspect_preview(&row, &ctx);
        assert!(refusal.contains("renders too long to preview"), "{refusal}");
        assert!(
            refusal.starts_with("Template for /\\_\\_forged\\_\\_ renders"),
            "{refusal}"
        );
        assert!(
            refusal.contains("/command name:\\_\\_forged\\_\\_ template:..."),
            "{refusal}"
        );
        assert!(!refusal.contains("__forged__"), "{refusal}");

        let list = format_command_list(&[row]);
        assert!(
            list.starts_with("/\\_\\_forged\\_\\_ (or !\\_\\_forged\\_\\_) — on"),
            "{list}"
        );
        assert!(list.encode_utf16().count() <= MAX_RENDERED_CHARS);

        let plain = format_command_list(&[inspect_row()]);
        assert!(plain.starts_with("/faq (or !faq) — on"), "{plain}");
    }
}
