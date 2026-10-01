//! Versioned JSON mapping shared by the legacy copy and verification tools.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingSpec {
    pub version: u32,
    pub tables: Vec<TableMapping>,
    #[serde(default)]
    pub pending_groups: Vec<PendingGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingGroup {
    pub group: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableMapping {
    pub group: String,
    pub source: String,
    pub target: String,
    /// Source key columns, in tuple order.
    pub keys: Vec<String>,
    /// Corresponding target conflict/key columns, in the same tuple order.
    pub conflict: Vec<String>,
    pub columns: Vec<ColumnMapping>,
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Upsert,
    InsertOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnMapping {
    pub source: String,
    pub target: String,
    /// Whitelisted PostgreSQL cast, applied on BOTH sides before comparison.
    pub pg_type: String,
}

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("invalid mapping JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid mapping: {0}")]
    Invalid(String),
}

impl MappingSpec {
    pub fn parse(json: &str) -> Result<Self, MappingError> {
        let spec: Self = serde_json::from_str(json)?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), MappingError> {
        if self.version != 1 || (self.tables.is_empty() && self.pending_groups.is_empty()) {
            return invalid(
                "version must be 1 and at least one ready or pending group is required",
            );
        }
        let mut pairs = BTreeSet::new();
        let mut groups = BTreeSet::new();
        for table in &self.tables {
            if table.group.is_empty() {
                return invalid("table group must not be empty");
            }
            groups.insert(table.group.as_str());
            quote_table(&table.source)?;
            quote_table(&table.target)?;
            if !pairs.insert((&table.source, &table.target)) {
                return invalid("duplicate source/target table mapping");
            }
            if table.columns.is_empty()
                || table.keys.is_empty()
                || table.keys.len() != table.conflict.len()
            {
                return invalid("columns and paired source/target key tuples are required");
            }
            let mut source_columns = BTreeSet::new();
            let mut target_columns = BTreeSet::new();
            for column in &table.columns {
                quote_identifier(&column.source)?;
                quote_identifier(&column.target)?;
                if !source_columns.insert(&column.source) || !target_columns.insert(&column.target)
                {
                    return invalid("duplicate mapped column");
                }
                if !matches!(
                    column.pg_type.as_str(),
                    "text"
                        | "bigint"
                        | "integer"
                        | "smallint"
                        | "boolean"
                        | "timestamptz"
                        | "timestamp"
                        | "date"
                        | "jsonb"
                        | "json"
                        | "numeric"
                        | "uuid"
                        | "bytea"
                ) {
                    return invalid("unsupported pg_type (arbitrary SQL is forbidden)");
                }
            }
            let mut keys = BTreeSet::new();
            let mut conflicts = BTreeSet::new();
            for (source, target) in table.keys.iter().zip(&table.conflict) {
                if !keys.insert(source) || !conflicts.insert(target) {
                    return invalid("duplicate key component");
                }
                let Some(column) = table.columns.iter().find(|c| &c.source == source) else {
                    return invalid("source key must be a mapped column");
                };
                if &column.target != target || matches!(column.pg_type.as_str(), "json" | "jsonb") {
                    return invalid("keys must pair the same mapped scalar column");
                }
            }
        }
        let mut pending = BTreeSet::new();
        for group in &self.pending_groups {
            if group.group.is_empty()
                || group.reason.is_empty()
                || groups.contains(group.group.as_str())
                || !pending.insert(&group.group)
            {
                return invalid("pending groups need unique names, reasons and no ready tables");
            }
        }
        Ok(())
    }

    /// A pending or misspelled group is a refusal, never a silent skip.
    pub fn select(&self, requested: &[String]) -> Result<Vec<&TableMapping>, MappingError> {
        self.validate()?;
        for pending in &self.pending_groups {
            if requested.is_empty() || requested.contains(&pending.group) {
                return invalid(&format!(
                    "pending group {}: {}",
                    pending.group, pending.reason
                ));
            }
        }
        for group in requested {
            if !self.tables.iter().any(|t| &t.group == group) {
                return invalid(&format!("unknown group: {group}"));
            }
        }
        Ok(self
            .tables
            .iter()
            .filter(|t| requested.is_empty() || requested.contains(&t.group))
            .collect())
    }
}

fn invalid<T>(message: &str) -> Result<T, MappingError> {
    Err(MappingError::Invalid(message.to_owned()))
}

pub(crate) fn quote_identifier(name: &str) -> Result<String, MappingError> {
    let mut bytes = name.bytes();
    if name.len() > 63
        || !bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return invalid("identifiers must be ASCII PostgreSQL names (maximum 63 bytes)");
    }
    Ok(format!("\"{name}\""))
}

pub(crate) fn quote_table(name: &str) -> Result<String, MappingError> {
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() > 2 {
        return invalid("tables must be table or schema.table");
    }
    parts
        .into_iter()
        .map(quote_identifier)
        .collect::<Result<Vec<_>, _>>()
        .map(|p| p.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn fixture() -> MappingSpec {
        MappingSpec::parse(include_str!("../mappings/example.json")).unwrap()
    }

    #[test]
    fn example_round_trips_and_selects() {
        let spec = fixture();
        let round_trip = MappingSpec::parse(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(round_trip.select(&[]).unwrap().len(), 1);
        assert_eq!(round_trip.tables[0].conflict_policy, ConflictPolicy::Upsert);
        assert!(round_trip.select(&["typo".into()]).is_err());
    }

    #[test]
    fn refuses_injection_duplicates_and_unpaired_keys() {
        for change in 0..5 {
            let mut spec = fixture();
            match change {
                0 => spec.tables[0].source = "members; DELETE FROM members".into(),
                1 => spec.tables[0].columns[0].pg_type = "text); DELETE FROM members;--".into(),
                2 => spec.tables[0].conflict = vec!["enabled".into()],
                3 => {
                    let duplicate = spec.tables[0].columns[0].clone();
                    spec.tables[0].columns.push(duplicate);
                }
                _ => spec.tables[0].keys.push("id".into()),
            }
            assert!(spec.validate().is_err());
        }
    }

    #[test]
    fn pending_groups_are_not_skipped() {
        let mut spec = fixture();
        spec.pending_groups.push(PendingGroup {
            group: "tickets".into(),
            reason: "schema not merged".into(),
        });
        assert!(spec
            .select(&[])
            .unwrap_err()
            .to_string()
            .contains("tickets"));
        assert!(spec.select(&["tickets".into()]).is_err());
        assert_eq!(spec.select(&["fixture".into()]).unwrap().len(), 1);
    }
}
