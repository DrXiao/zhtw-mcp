//! Per-project ignore terms.
//!
//! A term the user has declared uninteresting stays visible but stops
//! failing the build: severity drops to Info, which takes it out of both
//! the error and the warning gate.  Shared by the MCP `ignore_terms`
//! argument and the CLI `ignore_terms` config key so the two front ends
//! cannot disagree about what ignoring a term means.

use crate::rules::ruleset::{Issue, Severity};
use std::collections::HashSet;

/// Downgrade issues whose found term matches the ignore set to Info.
pub fn apply_ignore_set(issues: &mut [Issue], ignore_set: &HashSet<&str>) {
    if ignore_set.is_empty() {
        return;
    }
    for issue in issues {
        if ignore_set.contains(issue.found.as_str()) {
            issue.severity = Severity::Info;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(found: &str, severity: Severity) -> Issue {
        Issue::new(
            0,
            found.len(),
            found,
            vec!["x".to_string()],
            crate::rules::ruleset::IssueType::CrossStrait,
            severity,
        )
    }

    #[test]
    fn ignored_term_drops_to_info() {
        let mut issues = vec![
            issue("軟件", Severity::Warning),
            issue("內存", Severity::Warning),
        ];
        let set: HashSet<&str> = ["軟件"].into_iter().collect();
        apply_ignore_set(&mut issues, &set);
        assert_eq!(issues[0].severity, Severity::Info);
        assert_eq!(issues[1].severity, Severity::Warning);
    }

    #[test]
    fn empty_set_changes_nothing() {
        let mut issues = vec![issue("軟件", Severity::Error)];
        apply_ignore_set(&mut issues, &HashSet::new());
        assert_eq!(issues[0].severity, Severity::Error);
    }
}
