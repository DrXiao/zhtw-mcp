// Case rule scan using Aho-Corasick (case-insensitive).
//
// Checks that terms like "JavaScript", "TypeScript", "API" are correctly cased,
// rejecting matches that are already in canonical or alternative form.

use super::emit::Emitter;
use crate::engine::excluded::is_excluded;
use crate::rules::ruleset::{Issue, IssueType, Severity};

use super::Scanner;

impl Scanner {
    /// Case rule scan using Aho-Corasick (case-insensitive).
    ///
    /// For each match, check:
    /// 1. The matched text is NOT already in a valid form (canonical term
    ///    or one of the listed alternatives).
    /// 2. The match has word boundaries: no adjacent ASCII alphanumeric or
    ///    identifier underscore on either side (nor a hyphen, when the match is
    ///    all lowercase like a package name), and no dot that joins
    ///    it to another hostname or file-name label. Markdown emphasis
    ///    delimiters remain lintable.
    pub(crate) fn scan_case(&self, em: &mut Emitter<'_>) {
        let text = em.text;
        let excluded = em.excluded;
        let issues = &mut *em.issues;

        let case_ac = match self.case_ac.as_ref() {
            Some(ac) => ac,
            None => return,
        };
        let bytes = text.as_bytes();

        for mat in case_ac.find_iter(text) {
            let start = mat.start();
            let end = mat.end();

            if is_excluded(start, end, excluded) {
                continue;
            }

            let found = &text[start..end];
            let rule = &self.case_rules[mat.pattern().as_usize()];

            // Check if the matched text is already correct (canonical or
            // alternative).
            if found == rule.term {
                continue;
            }
            if let Some(ref alts) = rule.alternatives {
                if alts.iter().any(|a| a == found) {
                    continue;
                }
            }

            // A hyphen joins a package name only when the match is all
            // lowercase (typescript-eslint); Github-hosted is prose miscased.
            let hyphen_joins = !found.bytes().any(|b| b.is_ascii_uppercase());
            let in_identifier =
                |b: u8| b.is_ascii_alphanumeric() || b == b'_' || (hyphen_joins && b == b'-');

            // Glued to an identifier character on either side means the match
            // is part of a name. Symmetric underscore runs bounded by prose are
            // Markdown emphasis, not identifier separators, so they still lint.
            let glued = |s: usize, e: usize| {
                matches!(bytes[..s], [.., b] if in_identifier(b))
                    || matches!(bytes[e..], [b, ..] if in_identifier(b))
            };
            let before = bytes[..start]
                .iter()
                .rev()
                .take_while(|&&b| b == b'_')
                .count();
            let after = bytes[end..].iter().take_while(|&&b| b == b'_').count();
            let emphasis =
                before == after && (1..=3).contains(&before) && !glued(start - before, end + after);
            if glued(start, end) && !emphasis {
                continue;
            }

            // A dot joined to an alphanumeric on its far side makes the match
            // one label of a hostname or file name (openai.com, api.github.com,
            // python.exe), and those are case-sensitive or lowercase by rule. A
            // sentence-ending dot is followed by space or CJK and still lints.
            if matches!(bytes[..start], [.., a, b'.'] if a.is_ascii_alphanumeric())
                || matches!(bytes[end..], [b'.', b, ..] if b.is_ascii_alphanumeric())
            {
                continue;
            }

            issues.push(Issue::new(
                start,
                end - start,
                found,
                vec![rule.term.clone()],
                IssueType::Case,
                Severity::Info,
            ));
        }
    }
}
