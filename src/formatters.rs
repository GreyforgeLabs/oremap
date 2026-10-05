//! Report formatters.

use crate::models::InventoryReport;
use crate::privacy::sanitize_csv_cell;

/// Output formats accepted by `--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
    Csv,
}

impl Format {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "markdown" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }
}

pub const CSV_FIELDS: [&str; 9] = [
    "suggested_action",
    "kind",
    "name",
    "path",
    "status",
    "active",
    "enabled",
    "last_activity",
    "reason",
];

pub fn format_report(report: &InventoryReport, format: Format) -> String {
    match format {
        Format::Json => report.to_json(),
        Format::Markdown => markdown_report(report),
        Format::Csv => csv_report(report),
    }
}

fn escape(value: &str) -> String {
    value.replace('|', "\\|")
}

pub fn markdown_report(report: &InventoryReport) -> String {
    let mut lines: Vec<String> = vec![
        "# oremap Report".into(),
        String::new(),
        format!("- Generated: `{}`", report.generated_at),
        format!("- Host: `{}`", report.host),
        format!("- Items: `{}`", report.items.len()),
        format!(
            "- Complete: `{}`",
            if report.complete { "yes" } else { "no" }
        ),
        String::new(),
        "## Summary".into(),
        String::new(),
        "| Metric | Count |".into(),
        "|---|---:|".into(),
    ];
    let mut summary = report.summary();
    summary.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in summary {
        lines.push(format!("| `{}` | {value} |", escape(&key)));
    }
    if !report.warnings.is_empty() {
        lines.extend([String::new(), "## Warnings".into(), String::new()]);
        for warning in &report.warnings {
            lines.push(format!("- {}", escape(warning)));
        }
    }
    lines.extend([
        String::new(),
        "## Keep / Retire Matrix".into(),
        String::new(),
        "| Action | Kind | Name | Status | Last activity | Path | Reason |".into(),
        "|---|---|---|---|---|---|---|".into(),
    ]);
    for item in &report.items {
        let mut status_parts = Vec::new();
        if !item.status.is_empty() {
            status_parts.push(format!("status={}", item.status));
        }
        if !item.active.is_empty() {
            status_parts.push(format!("active={}", item.active));
        }
        if !item.enabled.is_empty() {
            status_parts.push(format!("enabled={}", item.enabled));
        }
        let cells = [
            escape(&item.suggested_action),
            escape(&item.kind),
            escape(&item.name),
            escape(&status_parts.join(", ")),
            escape(&item.last_activity),
            escape(&item.path),
            escape(&item.reason),
        ];
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn csv_field(out: &mut String, value: &str) {
    if value.chars().any(|c| matches!(c, ',' | '"' | '\r' | '\n')) {
        out.push('"');
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(value);
    }
}

/// One record in Python's default `excel` CSV dialect (`QUOTE_MINIMAL`,
/// `\r\n` terminator). A record made of a single empty field is written as
/// `""` so it cannot be mistaken for a blank line.
pub fn csv_record<S: AsRef<str>>(out: &mut String, fields: &[S]) {
    if fields.len() == 1 && fields[0].as_ref().is_empty() {
        out.push_str("\"\"\r\n");
        return;
    }
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        csv_field(out, field.as_ref());
    }
    out.push_str("\r\n");
}

pub fn csv_report(report: &InventoryReport) -> String {
    let mut out = String::with_capacity(128 + report.items.len() * 160);
    csv_record(&mut out, &CSV_FIELDS);
    for item in &report.items {
        let row: Vec<String> = CSV_FIELDS
            .iter()
            .map(|field| sanitize_csv_cell(item.field(field)))
            .collect();
        csv_record(&mut out, &row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{InventoryItem, InventoryReport};

    fn report_with(item: InventoryItem) -> InventoryReport {
        InventoryReport {
            schema: "service-cartographer.report.v1".into(),
            generated_at: "now".into(),
            host: "redacted".into(),
            roots: vec![],
            items: vec![item],
            warnings: vec![],
            complete: true,
        }
    }

    /// Minimal RFC 4180 reader used to check round-tripping, like
    /// `csv.DictReader` in the original tests.
    pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if quoted {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(c);
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
        if !field.is_empty() || !row.is_empty() {
            row.push(field);
            rows.push(row);
        }
        rows
    }

    // tests/test_formatters.py::test_csv_cell_sanitizer_neutralizes_formula_prefixes[*]
    #[test]
    fn test_csv_cell_sanitizer_neutralizes_formula_prefixes() {
        for value in [
            "=CMD()",
            "+1",
            "-1",
            "@SUM(A1:A2)",
            "\t=CMD()",
            "\r@SUM()",
            "  +1",
        ] {
            assert_eq!(sanitize_csv_cell(value), format!("'{value}"), "{value:?}");
        }
    }

    // tests/test_formatters.py::test_csv_cell_sanitizer_preserves_ordinary_values
    #[test]
    fn test_csv_cell_sanitizer_preserves_ordinary_values() {
        assert_eq!(sanitize_csv_cell("ordinary"), "ordinary");
    }

    // tests/test_formatters.py::test_csv_report_applies_policy_to_every_string_column[*]
    #[test]
    fn test_csv_report_applies_policy_to_every_string_column() {
        for field in CSV_FIELDS {
            let mut item = InventoryItem::new("wrapper", "demo");
            item.suggested_action = "review".into();
            item.path = "/tmp/demo".into();
            item.status = "present".into();
            item.last_activity = "2026-01-01T00:00:00Z".into();
            let poisoned = " \t=CMD()".to_string();
            match field {
                "suggested_action" => item.suggested_action = poisoned,
                "kind" => item.kind = poisoned,
                "name" => item.name = poisoned,
                "path" => item.path = poisoned,
                "status" => item.status = poisoned,
                "active" => item.active = poisoned,
                "enabled" => item.enabled = poisoned,
                "last_activity" => item.last_activity = poisoned,
                "reason" => item.reason = poisoned,
                _ => unreachable!(),
            }
            let rendered = format_report(&report_with(item), Format::Csv);
            let rows = parse_csv(&rendered);
            let column = CSV_FIELDS.iter().position(|name| *name == field).unwrap();
            assert_eq!(rows[0], CSV_FIELDS.map(String::from).to_vec());
            assert_eq!(rows[1][column], "' \t=CMD()", "{field}");
        }
    }

    // tests/test_formatters.py::test_json_remains_exact_machine_format
    #[test]
    fn test_json_remains_exact_machine_format() {
        let mut item = InventoryItem::new("wrapper", "=CMD()");
        item.path = "\t@SUM()".into();
        let rendered = format_report(&report_with(item), Format::Json);
        assert!(rendered.contains("\"name\": \"=CMD()\""));
        assert!(rendered.contains("\"path\": \"\\t@SUM()\""));
    }

    #[test]
    fn exact_json_layout_matches_python() {
        let mut item = InventoryItem::new("wrapper", "demo");
        item.metadata.set("size_bytes", "3");
        item.metadata.set("shebang", "#!/bin/sh");
        let mut report = report_with(item);
        report.roots = vec!["~/bin".into()];
        report.warnings = vec!["w".into()];
        report.complete = false;
        let expected = r##"{
  "complete": false,
  "generated_at": "now",
  "host": "redacted",
  "items": [
    {
      "active": "",
      "enabled": "",
      "kind": "wrapper",
      "last_activity": "",
      "metadata": {
        "shebang": "#!/bin/sh",
        "size_bytes": "3"
      },
      "name": "demo",
      "path": "",
      "reason": "",
      "status": "",
      "suggested_action": "review"
    }
  ],
  "roots": [
    "~/bin"
  ],
  "schema": "service-cartographer.report.v1",
  "summary": {
    "action:review": 1,
    "total": 1,
    "wrapper": 1
  },
  "warnings": [
    "w"
  ]
}
"##;
        assert_eq!(format_report(&report, Format::Json), expected);
    }

    #[test]
    fn markdown_escapes_pipes_and_lists_warnings() {
        let mut item = InventoryItem::new("wrapper", "a|b");
        item.status = "executable".into();
        item.active = "x".into();
        let mut report = report_with(item);
        report.warnings = vec!["bad | thing".into()];
        report.complete = false;
        let rendered = markdown_report(&report);
        assert!(rendered.contains("- Complete: `no`"));
        assert!(rendered.contains("## Warnings\n\n- bad \\| thing\n"));
        assert!(
            rendered
                .contains("| review | wrapper | a\\|b | status=executable, active=x |  |  |  |")
        );
        assert!(rendered.ends_with(" |\n"));
    }

    #[test]
    fn csv_quotes_minimally_with_crlf() {
        let mut out = String::new();
        csv_record(&mut out, &["a,b", "say \"hi\"", "line\nbreak", "plain"]);
        assert_eq!(out, "\"a,b\",\"say \"\"hi\"\"\",\"line\nbreak\",plain\r\n");
        let mut single = String::new();
        csv_record(&mut single, &[""]);
        assert_eq!(single, "\"\"\r\n");
    }
}
