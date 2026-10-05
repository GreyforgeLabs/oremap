//! Conservative keep/review/retire hints for discovered artifacts.

use crate::models::InventoryItem;
use crate::timefmt::{Micros, age_days};

/// Annotate inventory items with conservative action hints.
///
/// Old timestamps never imply retirement: only an operator-supplied retire
/// keyword can produce `retire_candidate`, and active/enabled units or dirty
/// repositories always win over that hint.
pub fn classify_items(
    items: &mut [InventoryItem],
    stale_days: i64,
    retire_keywords: &[String],
    keep_keywords: &[String],
    now: Micros,
) {
    let retire_terms: Vec<String> = retire_keywords.iter().map(|t| t.to_lowercase()).collect();
    let keep_terms: Vec<String> = keep_keywords.iter().map(|t| t.to_lowercase()).collect();

    for item in items.iter_mut() {
        let text = item.searchable_text();
        if keep_terms.iter().any(|term| text.contains(term.as_str())) {
            item.suggested_action = "keep_candidate".to_string();
            item.reason = "matched keep keyword".to_string();
            continue;
        }
        let enabled = item.enabled.to_lowercase();
        let active = item.active.to_lowercase();
        let status = item.status.to_lowercase();
        let age = age_days(&item.last_activity, now);

        let (action, reason) = if matches!(active.as_str(), "active" | "running")
            || matches!(enabled.as_str(), "enabled" | "static" | "generated")
        {
            ("keep_candidate", "active or enabled".to_string())
        } else if item.kind == "repository" && status == "dirty" {
            ("keep_candidate", "repository has local changes".to_string())
        } else if retire_terms.iter().any(|term| text.contains(term.as_str())) {
            (
                "retire_candidate",
                "matched operator-supplied retire keyword".to_string(),
            )
        } else if item.kind == "cron_job" {
            (
                "review",
                "scheduled command needs owner decision".to_string(),
            )
        } else if let Some(days) = age.filter(|days| *days >= stale_days) {
            (
                "review",
                format!("source timestamp is {days} days old; verify actual use"),
            )
        } else {
            ("review", "needs owner decision".to_string())
        };
        item.suggested_action = action.to_string();
        item.reason = reason;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timefmt::parse_iso;

    fn jan_2026() -> Micros {
        parse_iso("2026-01-01T00:00:00+00:00").unwrap()
    }

    fn run(item: &mut InventoryItem, retire: &[&str], keep: &[&str]) {
        let retire: Vec<String> = retire.iter().map(|s| s.to_string()).collect();
        let keep: Vec<String> = keep.iter().map(|s| s.to_string()).collect();
        classify_items(std::slice::from_mut(item), 90, &retire, &keep, jan_2026());
    }

    // tests/test_classifier.py::test_active_unit_is_keep_candidate
    #[test]
    fn test_active_unit_is_keep_candidate() {
        let mut item = InventoryItem::new("systemd_unit", "demo.service");
        item.active = "active".into();
        run(&mut item, &[], &[]);
        assert_eq!(item.suggested_action, "keep_candidate");
    }

    // tests/test_classifier.py::test_old_source_timestamp_never_implies_retirement
    #[test]
    fn test_old_source_timestamp_never_implies_retirement() {
        let mut item = InventoryItem::new("wrapper", "old-tool");
        item.last_activity = "2025-01-01T00:00:00+00:00".into();
        run(&mut item, &[], &[]);
        assert_eq!(item.suggested_action, "review");
        assert!(item.reason.contains("verify actual use"));
        assert_eq!(
            item.reason,
            "source timestamp is 365 days old; verify actual use"
        );
    }

    // tests/test_classifier.py::test_active_legacy_unit_is_kept_even_with_retire_keyword
    #[test]
    fn test_active_legacy_unit_is_kept_even_with_retire_keyword() {
        let mut item = InventoryItem::new("systemd_unit", "legacy.service");
        item.active = "active".into();
        run(&mut item, &["legacy"], &[]);
        assert_eq!(item.suggested_action, "keep_candidate");
    }

    // tests/test_classifier.py::test_operator_retire_keyword_marks_inactive_item
    #[test]
    fn test_operator_retire_keyword_marks_inactive_item() {
        let mut item = InventoryItem::new("wrapper", "retire-this");
        item.status = "executable".into();
        run(&mut item, &["retire-this"], &[]);
        assert_eq!(item.suggested_action, "retire_candidate");
    }

    #[test]
    fn precedence_matches_reference_order() {
        let mut dirty = InventoryItem::new("repository", "repo");
        dirty.status = "dirty".into();
        run(&mut dirty, &["repo"], &[]);
        assert_eq!(dirty.reason, "repository has local changes");

        let mut cron = InventoryItem::new("cron_job", "user-crontab:1");
        run(&mut cron, &[], &[]);
        assert_eq!(cron.reason, "scheduled command needs owner decision");

        let mut kept = InventoryItem::new("cron_job", "user-crontab:2");
        run(&mut kept, &["cron"], &["CRON"]);
        assert_eq!(kept.reason, "matched keep keyword");

        let mut empty_keep = InventoryItem::new("wrapper", "x");
        run(&mut empty_keep, &[], &[""]);
        assert_eq!(empty_keep.suggested_action, "keep_candidate");

        let mut fresh = InventoryItem::new("wrapper", "fresh");
        fresh.last_activity = "2025-12-30T00:00:00+00:00".into();
        run(&mut fresh, &[], &[]);
        assert_eq!(fresh.reason, "needs owner decision");
    }
}
