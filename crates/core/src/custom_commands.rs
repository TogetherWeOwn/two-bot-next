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
/// order (legacy `placeholdersIn`).
#[must_use]
pub fn placeholders_in(template: &str) -> Vec<String> {
    let bytes = template.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
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
/// ceiling is refused.
pub fn render_template(template: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = placeholder_end(bytes, i) {
                let key = &template[i + 1..end - 1];
                match key {
                    "user" => out.push_str(&ctx.user),
                    "username" => out.push_str(&ctx.username),
                    "server" => out.push_str(&ctx.server),
                    "channel" => out.push_str(&ctx.channel),
                    _ => out.push_str(&template[i..end]),
                }
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
    #[error("Command name \"{0}\" is reserved by Owen.")]
    ReservedName(String),
    #[error("Description must be between 1 and 100 characters.")]
    InvalidDescription,
    #[error("{0}")]
    InvalidTemplate(#[from] TemplateError),
    #[error("Text trigger must match ^![a-z0-9_-]{{1,32}}$.")]
    InvalidTextTrigger,
    #[error("Text trigger must be lowercase.")]
    TextTriggerNotLowercase,
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
            target_key: Some(name.to_owned()),
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
            target_key: Some(name.to_owned()),
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
            target_key: Some(name.to_owned()),
            outcome: (if deleted { "ok" } else { "absent" }).to_owned(),
            reason: None,
        }
    }

    #[must_use]
    pub fn run(guild_id: &str, actor_id: &str, name: &str, ok: bool, reason: Option<&str>) -> Self {
        Self {
            guild_id: guild_id.to_owned(),
            actor_id: Some(actor_id.to_owned()),
            action: "command.run".to_owned(),
            target_key: Some(name.to_owned()),
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

/// `/command-list` reply lines (legacy `discord.ts`): one
/// `/name [(or !trigger)] — on|off` per row, capped at Discord's ceiling.
#[must_use]
pub fn format_command_list(rows: &[StoredCommand]) -> String {
    if rows.is_empty() {
        return "No custom commands defined.".to_owned();
    }
    let body = rows
        .iter()
        .map(|r| {
            let trigger = r
                .text_trigger
                .as_deref()
                .map(|t| format!(" (or {t})"))
                .unwrap_or_default();
            format!(
                "/{}{} — {}",
                r.name,
                trigger,
                if r.enabled { "on" } else { "off" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut units = 0;
    body.chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= MAX_RENDERED_CHARS
        })
        .collect()
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
        assert_eq!(reserved, published);
        assert!(reserved.contains("templateassistant"));
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
            GUILD_COMMAND_LIMIT
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
            "/faq — on\n/rules (or !rules) — off"
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
}
