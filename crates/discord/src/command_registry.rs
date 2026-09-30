//! Guild command drift: canonical writable fields, stable hashes and a pure diff.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::de::Error as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use twilight_model::{application::command::Command, guild::Permissions};

/// Twilight's Permissions deserializer truncates bits unknown to its pinned
/// model. Restore the raw u64 so future Discord permission bits remain drift.
pub(crate) fn decode_guild_commands(body: &[u8]) -> Result<Vec<Command>, serde_json::Error> {
    let values: Vec<Value> = serde_json::from_slice(body)?;
    values
        .into_iter()
        .map(|value| {
            let bits = match value.get("default_member_permissions") {
                None | Some(Value::Null) => None,
                Some(Value::String(raw)) => {
                    Some(raw.parse::<u64>().map_err(serde_json::Error::custom)?)
                }
                Some(_) => {
                    return Err(serde_json::Error::custom(
                        "default_member_permissions must be a decimal string or null",
                    ))
                }
            };
            let mut command: Command = serde_json::from_value(value)?;
            command.default_member_permissions = bits.map(Permissions::from_bits_retain);
            Ok(command)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistrySnapshot {
    pub hash: String,
    pub commands: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldChange {
    pub path: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegistryDiff {
    pub current_hash: String,
    pub compiled_hash: String,
    pub added: BTreeMap<String, Value>,
    pub removed: BTreeMap<String, Value>,
    pub changed: BTreeMap<String, Vec<FieldChange>>,
}

impl RegistryDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Deterministic operator output, including old/new values for nested fields.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "current hash: {}\ncompiled hash: {}\nadded: {} removed: {} changed: {}\n",
            self.current_hash,
            self.compiled_hash,
            self.added.len(),
            self.removed.len(),
            self.changed.len()
        );
        for (name, command) in &self.added {
            let _ = writeln!(out, "+ {name}: {command}");
        }
        for (name, command) in &self.removed {
            let _ = writeln!(out, "- {name}: {command}");
        }
        for (name, fields) in &self.changed {
            let _ = writeln!(out, "~ {name}");
            for field in fields {
                let _ = writeln!(out, "  {}: {} -> {}", field.path, field.before, field.after);
            }
        }
        if self.is_empty() {
            out.push_str("No command drift.\n");
        }
        out
    }
}

/// Server IDs/versions and global-only settings cannot cause guild drift.
/// Command ordering and object-key ordering are insignificant; option/choice
/// ordering is significant and must never be sorted away.
pub fn registry_snapshot(commands: &[Command]) -> Result<RegistrySnapshot, serde_json::Error> {
    let mut normalized = BTreeMap::new();
    for command in commands {
        let mut value = serde_json::to_value(command)?;
        if let Some(object) = value.as_object_mut() {
            for key in [
                "application_id",
                "guild_id",
                "id",
                "version",
                "contexts",
                "integration_types",
                "dm_permission",
            ] {
                object.remove(key);
            }
        }
        let value = normalize(value);
        // Command names are unique within their type, not across all types.
        let key = format!("{}/{}", value["type"], command.name);
        normalized.insert(key, value);
    }
    let bytes = serde_json::to_vec(&normalized)?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    Ok(RegistrySnapshot {
        hash,
        commands: normalized,
    })
}

fn normalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object
                .into_iter()
                .filter_map(|(key, value)| {
                    // Discord fills in default false and empty collections on GET.
                    // Null permissions mean everyone; the decimal bitfield "0"
                    // means administrators only and is deliberately retained.
                    let empty = value.is_null()
                        || matches!(key.as_str(), "required" | "autocomplete" | "nsfw")
                            && value == Value::Bool(false)
                        || matches!(key.as_str(), "options" | "choices" | "channel_types")
                            && value.as_array().is_some_and(Vec::is_empty)
                        || key.ends_with("_localizations")
                            && value.as_object().is_some_and(serde_json::Map::is_empty);
                    if empty {
                        None
                    } else {
                        Some((key, normalize(value)))
                    }
                })
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(normalize).collect()),
        other => other,
    }
}

pub fn diff_commands(
    current: &[Command],
    compiled: &[Command],
) -> Result<RegistryDiff, serde_json::Error> {
    let current = registry_snapshot(current)?;
    let compiled = registry_snapshot(compiled)?;
    let mut diff = RegistryDiff {
        current_hash: current.hash,
        compiled_hash: compiled.hash,
        added: BTreeMap::new(),
        removed: BTreeMap::new(),
        changed: BTreeMap::new(),
    };
    for (key, after) in &compiled.commands {
        match current.commands.get(key) {
            None => {
                diff.added.insert(key.clone(), after.clone());
            }
            Some(before) if before != after => {
                let mut fields = Vec::new();
                field_changes("", before, after, &mut fields);
                diff.changed.insert(key.clone(), fields);
            }
            Some(_) => {}
        }
    }
    for (key, before) in current.commands {
        if !compiled.commands.contains_key(&key) {
            diff.removed.insert(key, before);
        }
    }
    Ok(diff)
}

fn field_changes(path: &str, before: &Value, after: &Value, out: &mut Vec<FieldChange>) {
    if before == after {
        return;
    }
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                field_changes(
                    &path,
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for i in 0..a.len().max(b.len()) {
                field_changes(
                    &format!("{path}[{i}]"),
                    a.get(i).unwrap_or(&Value::Null),
                    b.get(i).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ => out.push(FieldChange {
            path: path.to_owned(),
            before: before.clone(),
            after: after.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(extra: Value) -> Command {
        let mut base = json!({"type": 1, "name": "test", "description": "Test", "version": "1"});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        decode_guild_commands(&serde_json::to_vec(&vec![base]).unwrap())
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn table_defaults_permissions_option_order_and_choices() {
        let option = json!({"type": 3, "name": "first", "description": "First", "required": false});
        let second =
            json!({"type": 3, "name": "second", "description": "Second", "required": false});
        let choices = json!([
            {"name": "A", "value": "a"}, {"name": "B", "value": "b"}
        ]);
        let cases = [
            (
                "server metadata",
                json!({}),
                json!({"id":"2", "application_id":"3", "guild_id":"4", "version":"5"}),
                false,
            ),
            (
                "API defaults",
                json!({}),
                json!({"nsfw":false, "options":[], "name_localizations":{}, "default_member_permissions":null}),
                false,
            ),
            (
                "permissions",
                json!({"default_member_permissions":"4"}),
                json!({"default_member_permissions":"8"}),
                true,
            ),
            (
                "future permission bit",
                json!({"default_member_permissions":"4"}),
                json!({"default_member_permissions":"281474976710660"}),
                true,
            ),
            (
                "permission decimal normalization",
                json!({"default_member_permissions":"0004"}),
                json!({"default_member_permissions":"4"}),
                false,
            ),
            (
                "no permissions vs zero",
                json!({}),
                json!({"default_member_permissions":"0"}),
                true,
            ),
            (
                "option order",
                json!({"options":[option.clone(), second.clone()]}),
                json!({"options":[second, option.clone()]}),
                true,
            ),
            ("description", json!({}), json!({"description":"New"}), true),
            (
                "choices",
                json!({"options":[{"type":3,"name":"first","description":"First","choices":choices}]}),
                json!({"options":[{"type":3,"name":"first","description":"First","choices":[{"name":"A","value":"changed"}]}]}),
                true,
            ),
            (
                "choice order",
                json!({"options":[{"type":3,"name":"first","description":"First","choices":[{"name":"A","value":"a"},{"name":"B","value":"b"}]}]}),
                json!({"options":[{"type":3,"name":"first","description":"First","choices":[{"name":"B","value":"b"},{"name":"A","value":"a"}]}]}),
                true,
            ),
        ];
        for (name, a, b, changed) in cases {
            let a = command(a);
            let b = command(b);
            let diff = diff_commands(&[a], &[b]).unwrap();
            assert_eq!(!diff.is_empty(), changed, "{name}");
            assert_eq!(diff.current_hash != diff.compiled_hash, changed, "{name}");
        }
    }

    #[test]
    fn command_order_and_localization_key_order_do_not_change_hash() {
        let first = command(json!({"name_localizations":{"de":"Test", "fr":"Essai"}}));
        let reordered = command(json!({"name_localizations":{"fr":"Essai", "de":"Test"}}));
        let second = command(json!({"name":"other"}));
        assert!(
            diff_commands(&[first, second.clone()], &[second, reordered])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reports_add_remove_and_nested_old_new_values() {
        let old = command(json!({"description":"Old"}));
        let new = command(json!({"description":"New"}));
        let gone = command(json!({"name":"gone"}));
        let added = command(json!({"name":"added"}));
        let diff = diff_commands(&[old, gone], &[new, added]).unwrap();
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.changed.len(), 1);
        let rendered = diff.render();
        assert!(rendered.contains("+ 1/added"));
        assert!(rendered.contains("- 1/gone"));
        assert!(rendered.contains("description: \"Old\" -> \"New\""));
    }
}
