//! Core data structures for oremap.

use crate::pyfmt::push_json_string;

/// Insertion-ordered string map with Python `dict` assignment semantics:
/// assigning an existing key replaces the value in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    entries: Vec<(String, String)>,
}

impl Metadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        let mut metadata = Self::new();
        for (key, value) in pairs {
            metadata.set(key, value);
        }
        metadata
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if let Some(entry) = self.entries.iter_mut().find(|(name, _)| *name == key) {
            entry.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One discovered automation or workflow artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryItem {
    pub kind: String,
    pub name: String,
    pub path: String,
    pub status: String,
    pub active: String,
    pub enabled: String,
    pub last_activity: String,
    pub suggested_action: String,
    pub reason: String,
    pub metadata: Metadata,
}

impl InventoryItem {
    pub fn new(kind: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            path: String::new(),
            status: String::new(),
            active: String::new(),
            enabled: String::new(),
            last_activity: String::new(),
            suggested_action: "review".to_string(),
            reason: String::new(),
            metadata: Metadata::new(),
        }
    }

    /// Lowercased text used for focus filters and keyword classification.
    pub fn searchable_text(&self) -> String {
        let metadata = self
            .metadata
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        let parts = [
            self.kind.as_str(),
            self.name.as_str(),
            self.path.as_str(),
            self.status.as_str(),
            self.active.as_str(),
            self.enabled.as_str(),
            self.reason.as_str(),
            metadata.as_str(),
        ];
        parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// Value of one of the CSV string columns by name.
    pub fn field(&self, name: &str) -> &str {
        match name {
            "kind" => &self.kind,
            "name" => &self.name,
            "path" => &self.path,
            "status" => &self.status,
            "active" => &self.active,
            "enabled" => &self.enabled,
            "last_activity" => &self.last_activity,
            "suggested_action" => &self.suggested_action,
            "reason" => &self.reason,
            _ => "",
        }
    }
}

/// A complete scan result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryReport {
    pub schema: String,
    pub generated_at: String,
    pub host: String,
    pub roots: Vec<String>,
    pub items: Vec<InventoryItem>,
    pub warnings: Vec<String>,
    pub complete: bool,
}

impl InventoryReport {
    /// Counts in first-seen order: `total`, then each kind and action key.
    pub fn summary(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = vec![("total".to_string(), self.items.len())];
        let mut bump = |key: String| {
            if let Some(entry) = counts.iter_mut().find(|(name, _)| *name == key) {
                entry.1 += 1;
            } else {
                counts.push((key, 1));
            }
        };
        for item in &self.items {
            bump(item.kind.clone());
            bump(format!("action:{}", item.suggested_action));
        }
        counts
    }

    /// `json.dumps(report.to_dict(), indent=2, sort_keys=True) + "\n"`.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(512 + self.items.len() * 512);
        out.push_str("{\n  \"complete\": ");
        out.push_str(if self.complete { "true" } else { "false" });
        out.push_str(",\n  \"generated_at\": ");
        push_json_string(&mut out, &self.generated_at);
        out.push_str(",\n  \"host\": ");
        push_json_string(&mut out, &self.host);
        out.push_str(",\n  \"items\": ");
        if self.items.is_empty() {
            out.push_str("[]");
        } else {
            out.push('[');
            for (index, item) in self.items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str("\n    ");
                push_item_json(&mut out, item);
            }
            out.push_str("\n  ]");
        }
        out.push_str(",\n  \"roots\": ");
        push_string_list(&mut out, &self.roots);
        out.push_str(",\n  \"schema\": ");
        push_json_string(&mut out, &self.schema);
        out.push_str(",\n  \"summary\": {");
        let mut summary = self.summary();
        summary.sort_by(|a, b| a.0.cmp(&b.0));
        for (index, (key, value)) in summary.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("\n    ");
            push_json_string(&mut out, key);
            out.push_str(": ");
            out.push_str(&value.to_string());
        }
        out.push_str("\n  },\n  \"warnings\": ");
        push_string_list(&mut out, &self.warnings);
        out.push_str("\n}\n");
        out
    }
}

fn push_string_list(out: &mut String, values: &[String]) {
    if values.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("\n    ");
        push_json_string(out, value);
    }
    out.push_str("\n  ]");
}

fn push_item_json(out: &mut String, item: &InventoryItem) {
    let field = |out: &mut String, key: &str, value: &str, last: bool| {
        out.push_str("\n      ");
        push_json_string(out, key);
        out.push_str(": ");
        push_json_string(out, value);
        if !last {
            out.push(',');
        }
    };
    out.push('{');
    field(out, "active", &item.active, false);
    field(out, "enabled", &item.enabled, false);
    field(out, "kind", &item.kind, false);
    field(out, "last_activity", &item.last_activity, false);
    out.push_str("\n      \"metadata\": ");
    if item.metadata.is_empty() {
        out.push_str("{}");
    } else {
        let mut pairs: Vec<(&str, &str)> = item.metadata.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        out.push('{');
        for (index, (key, value)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("\n        ");
            push_json_string(out, key);
            out.push_str(": ");
            push_json_string(out, value);
        }
        out.push_str("\n      }");
    }
    out.push(',');
    field(out, "name", &item.name, false);
    field(out, "path", &item.path, false);
    field(out, "reason", &item.reason, false);
    field(out, "status", &item.status, false);
    field(out, "suggested_action", &item.suggested_action, true);
    out.push_str("\n    }");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_assignment_keeps_first_position() {
        let mut metadata = Metadata::from_pairs([("scope", "user"), ("source", "unit_file")]);
        metadata.set("executable", "/usr/bin/systemctl");
        metadata.set("scope", "system");
        let keys: Vec<&str> = metadata.iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["scope", "source", "executable"]);
        assert_eq!(metadata.get("scope"), Some("system"));
    }

    #[test]
    fn searchable_text_skips_empty_parts() {
        let mut item = InventoryItem::new("wrapper", "Demo");
        item.metadata.set("Shebang", "#!/bin/SH");
        assert_eq!(item.searchable_text(), "wrapper demo shebang=#!/bin/sh");
    }

    #[test]
    fn summary_counts_kinds_and_actions() {
        let report = InventoryReport {
            schema: "s".into(),
            generated_at: "now".into(),
            host: "redacted".into(),
            roots: vec![],
            items: vec![
                InventoryItem::new("wrapper", "a"),
                InventoryItem::new("cron_job", "b"),
            ],
            warnings: vec![],
            complete: true,
        };
        assert_eq!(
            report.summary(),
            vec![
                ("total".to_string(), 2),
                ("wrapper".to_string(), 1),
                ("action:review".to_string(), 2),
                ("cron_job".to_string(), 1),
            ]
        );
    }
}
