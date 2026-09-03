// Output rendering for the lint subcommand: the typed shapes that go to stdout
// and the five formatters that fill them.
//
// Typed structs rather than a Value tree, so serialization does not allocate an
// intermediate document per file.

use std::io::IsTerminal;
use std::path::Path;

// ANSI color helpers for human-format output

/// Whether stderr supports ANSI colors.
pub(crate) fn use_color() -> bool {
    // Respect NO_COLOR env var (https://no-color.org/).
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stderr().is_terminal()
}

pub(crate) struct Colors {
    pub(crate) red: &'static str,
    pub(crate) yellow: &'static str,
    pub(crate) cyan: &'static str,
    pub(crate) dim: &'static str,
    pub(crate) bold: &'static str,
    pub(crate) reset: &'static str,
}

pub(crate) const COLORS_ON: Colors = Colors {
    red: "\x1b[31m",
    yellow: "\x1b[33m",
    cyan: "\x1b[36m",
    dim: "\x1b[2m",
    bold: "\x1b[1m",
    reset: "\x1b[0m",
};

pub(crate) const COLORS_OFF: Colors = Colors {
    red: "",
    yellow: "",
    cyan: "",
    dim: "",
    bold: "",
    reset: "",
};

/// The report settings the formatters read.
///
/// Four fields lifted out of the batch parameter block, so rendering does not
/// depend on the type that drives the batch: that dependency was the only
/// edge from here back into `lint`, and it made the two halves of the split
/// mutually dependent.
#[derive(Clone, Copy)]
pub(crate) struct RenderOpts<'a> {
    pub(crate) detect_style: bool,
    pub(crate) consistency: bool,
    pub(crate) explain: bool,
    pub(crate) glossary: &'a zhtw_mcp::rules::glossary::ProjectGlossary,
}

#[derive(Clone, Copy)]
pub(crate) enum LintFormat {
    Human,
    Json,
    Sarif,
    Compact,
    Tabular,
}

impl LintFormat {
    /// True when the report claims stdout, so a fixed document cannot also go
    /// there.  Human output goes to stderr; every other renderer prints.
    ///
    /// An exhaustive match, not a negated `matches!`: a sixth format then has
    /// to answer the question at compile time instead of defaulting into the
    /// passthrough branch and truncating a piped document.
    pub(crate) fn report_owns_stdout(self) -> bool {
        match self {
            LintFormat::Human => false,
            LintFormat::Json | LintFormat::Sarif | LintFormat::Compact | LintFormat::Tabular => {
                true
            }
        }
    }
}

// Typed output structs for direct serialization (no Value tree allocation).

#[derive(serde::Serialize)]
pub(crate) struct CliFileOutput {
    pub(crate) file: String,
    pub(crate) detected_script: String,
    pub(crate) issues: Vec<zhtw_mcp::rules::ruleset::Issue>,
    pub(crate) total: usize,
    pub(crate) errors: usize,
    pub(crate) warnings: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tm_suppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fixes_applied: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fixes_skipped: Option<usize>,
    /// Subset of fixes_skipped the fixer judged on the issue's own merits.
    /// fixes_skipped also counts issues that were never in scope for the tier,
    /// overlapped an earlier fix, or landed in an excluded region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fixes_declined: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ai_signature: Option<zhtw_mcp::engine::ai_score::AiSignatureReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) translationese_signature:
        Option<zhtw_mcp::engine::translationese_score::TranslationeseReport>,
    /// Composite style scorecard.  Three orthogonal axes, never
    /// collapsed into a single number.  Present only when --detect-style
    /// is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) style_scorecard: Option<zhtw_mcp::engine::style_score::StyleScorecard>,
    /// Document-wide consistency report.  Present only when
    /// --consistency is set AND mixed regional usage is detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) consistency: Option<zhtw_mcp::engine::consistency::ConsistencyReport>,
}

#[derive(serde::Serialize)]
struct SarifDocument<'a> {
    #[serde(rename = "$schema")]
    pub(crate) schema: &'static str,
    pub(crate) version: &'static str,
    pub(crate) runs: [SarifRun<'a>; 1],
}

#[derive(serde::Serialize)]
struct SarifRun<'a> {
    pub(crate) tool: SarifTool,
    pub(crate) results: &'a [SarifResult],
}

#[derive(serde::Serialize)]
struct SarifTool {
    pub(crate) driver: SarifDriver,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifDriver {
    pub(crate) name: &'static str,
    pub(crate) version: &'static str,
    pub(crate) information_uri: &'static str,
    pub(crate) rules: Vec<SarifRuleDef>,
}

/// SARIF consumers validate `informationUri` as a URI and drop the run if it is
/// not one, so an empty `repository` in Cargo.toml must not reach the output.
/// Fail the build instead of substituting a literal: a hardcoded fallback is
/// what pointed this field at the wrong GitHub org in the first place.
const SARIF_INFORMATION_URI: &str = env!("CARGO_PKG_REPOSITORY");
const _: () = assert!(
    !SARIF_INFORMATION_URI.is_empty(),
    "Cargo.toml must declare `repository`: it becomes SARIF informationUri"
);

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SarifRuleDef {
    id: String,
    short_description: SarifMessage,
}

#[derive(serde::Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SarifResult {
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: [SarifLocation; 1],
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    physical_location: SarifPhysicalLocation,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifArtifactLocation {
    uri: String,
    uri_base_id: &'static str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    start_line: usize,
    start_column: usize,
    byte_offset: usize,
    byte_length: usize,
}

/// Render a file argument as a `path:` prefix relative to the current
/// directory, or empty for stdin. Shared by the compact and tabular
/// formatters, which had byte-identical copies of this.
fn display_path_prefix(file_arg: &str) -> String {
    if file_arg == "--" {
        return String::new();
    }
    // The cwd goes through the same normalizer the path itself did in
    // `cli::discover`, or the two spellings of one directory do not match and
    // nothing is stripped.
    let display_path = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            Path::new(file_arg)
                .strip_prefix(super::discover::normalize_path(&cwd))
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| file_arg.to_string());
    format!("{display_path}:")
}

/// Join suggestions for display, rendering the delete case as `(delete)`.
fn format_suggestions(suggestions: &[String]) -> String {
    if zhtw_mcp::rules::ruleset::is_delete_suggestion(suggestions) {
        zhtw_mcp::rules::ruleset::DELETE_SUGGESTION.to_string()
    } else {
        suggestions.join(", ")
    }
}

/// One file's results, as handed to the output formatters.
pub(crate) struct FileReport<'a> {
    pub(crate) file_arg: &'a str,
    pub(crate) detected_script: &'a str,
    pub(crate) issues: &'a [zhtw_mcp::rules::ruleset::Issue],
    pub(crate) error_count: usize,
    pub(crate) warning_count: usize,
    pub(crate) tm_suppressed: usize,
    pub(crate) fixes_applied: Option<usize>,
    pub(crate) fixes_skipped: Option<usize>,
    pub(crate) fixes_declined: Option<usize>,
    pub(crate) ai_signature: Option<&'a zhtw_mcp::engine::ai_score::AiSignatureReport>,
    pub(crate) translationese_signature:
        Option<&'a zhtw_mcp::engine::translationese_score::TranslationeseReport>,
    /// Text the consistency report should run against: post-fix when fixes
    /// were written, original otherwise.
    pub(crate) consistency_text: &'a str,
    pub(crate) text_char_count: usize,
    pub(crate) multi: bool,
}

/// Build the JSON result object for one file.
pub(crate) fn render_json(r: &FileReport<'_>, params: RenderOpts<'_>) -> CliFileOutput {
    CliFileOutput {
        file: r.file_arg.to_string(),
        detected_script: r.detected_script.to_string(),
        total: r.issues.len(),
        issues: r.issues.to_vec(),
        errors: r.error_count,
        warnings: r.warning_count,
        tm_suppressed: (r.tm_suppressed > 0).then_some(r.tm_suppressed),
        fixes_applied: r.fixes_applied,
        fixes_skipped: r.fixes_skipped,
        fixes_declined: r.fixes_declined,
        ai_signature: r.ai_signature.cloned(),
        translationese_signature: r.translationese_signature.cloned(),
        style_scorecard: params.detect_style.then(|| {
            zhtw_mcp::engine::style_score::StyleScorecard::build(
                r.ai_signature,
                r.translationese_signature,
                r.issues,
                r.text_char_count,
            )
        }),
        consistency: params
            .consistency
            .then(|| {
                zhtw_mcp::engine::consistency::compute_consistency_report(
                    r.consistency_text,
                    r.issues,
                    params.glossary,
                )
            })
            .filter(|c| !c.is_empty()),
    }
}

/// Print one file's results in the default human format, to stderr.
pub(crate) fn render_human(r: &FileReport<'_>, params: RenderOpts<'_>, c: &Colors) {
    let prefix = if r.multi {
        format!("{}{}{}:", c.bold, r.file_arg, c.reset)
    } else {
        String::new()
    };
    if r.issues.is_empty() {
        eprintln!("{prefix}{}No issues found.{}", c.dim, c.reset);
    } else {
        for issue in r.issues {
            let sev_color = match issue.severity {
                zhtw_mcp::rules::ruleset::Severity::Error => c.red,
                zhtw_mcp::rules::ruleset::Severity::Warning => c.yellow,
                zhtw_mcp::rules::ruleset::Severity::Info => c.cyan,
            };
            let verify_tag = match issue.anchor_match {
                Some(true) => " [verified]",
                Some(false) => " [unverified]",
                None => "",
            };
            eprintln!(
                "{prefix}{}:{}: {}{}{} {}[{}]{} '{}{}{}' -> {}{}",
                issue.line,
                issue.col,
                sev_color,
                issue.severity.name(),
                c.reset,
                c.dim,
                issue.rule_type.name(),
                c.reset,
                c.bold,
                issue.found,
                c.reset,
                format_suggestions(&issue.suggestions),
                verify_tag,
            );
            if params.explain {
                if let Some(ctx) = &issue.context {
                    eprintln!("  {}context:{} {ctx}", c.dim, c.reset);
                }
                if let Some(eng) = &issue.english {
                    eprintln!("  {}english:{} {eng}", c.dim, c.reset);
                }
            }
        }
        eprintln!(
            "\n{prefix}{}{} issue(s) found.{}",
            c.bold,
            r.issues.len(),
            c.reset
        );
    }
    if let Some(sig) = r.ai_signature {
        render_score_line(&prefix, "AI score:", sig.score, &sig.top_signals, c);
    }
    if let Some(sig) = r.translationese_signature {
        render_score_line(&prefix, "翻譯腔 score:", sig.score, &sig.top_signals, c);
    }
}

/// Print one `score: N.NN (level)` line plus its top signals.
fn render_score_line(prefix: &str, label: &str, score: f32, signals: &[String], c: &Colors) {
    let level = if score >= 0.7 {
        "high"
    } else if score >= 0.4 {
        "medium"
    } else {
        "low"
    };
    eprintln!("{prefix}{}{label}{} {score:.2} ({level})", c.cyan, c.reset);
    for signal in signals {
        eprintln!("  {}{signal}{}", c.dim, c.reset);
    }
}

/// Print one file's results in grep-style compact format, deduplicated.
/// Format: `file:line:col:S:rule:from→to`.
pub(crate) fn render_compact(r: &FileReport<'_>, explain: bool) {
    use std::collections::HashMap;

    type CompactKey<'a> = (&'a str, &'a str, String, &'a str);
    struct CompactGroup {
        first_loc: (usize, usize),
        locs: Vec<(usize, usize)>,
        /// Rendered from the issue rather than rebuilt from the dedup key.
        /// The key is a bar-joined suggestion list, which is empty both for a
        /// deletion rule and for a rule with no suggestion at all, so
        /// splitting it back apart printed nothing after the arrow. The
        /// shared renderer keeps the delete sentinel and the english
        /// fallback that the human and tabular formats already show.
        suggestion: String,
        context: Option<String>,
        english: Option<String>,
    }

    // Group by dedup key, preserving first-occurrence order via index.
    let mut groups: HashMap<CompactKey<'_>, CompactGroup> = HashMap::new();
    let mut order: Vec<CompactKey<'_>> = Vec::new();
    for issue in r.issues {
        let key = issue.compact_dedup_key();
        let group = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            CompactGroup {
                first_loc: (issue.line, issue.col),
                locs: Vec::new(),
                suggestion: issue.compact_suggestion(),
                context: issue.context.as_deref().map(str::to_string),
                english: issue.english.as_deref().map(str::to_string),
            }
        });
        group.locs.push((issue.line, issue.col));
    }

    let file_prefix = display_path_prefix(r.file_arg);

    // Emit in source order (first occurrence of each group).
    order.sort_by_key(|k| groups[k].first_loc);
    for key in &order {
        let (found, rt, _sug_key, sev) = key;
        let group = &groups[key];
        let display_sug = &group.suggestion;
        if group.locs.len() == 1 {
            print!(
                "{file_prefix}{}:{}:{sev}:{rt}:{found}\u{2192}{display_sug}",
                group.locs[0].0, group.locs[0].1
            );
        } else {
            let rest: Vec<String> = group.locs[1..]
                .iter()
                .map(|(l, c)| format!("{l}:{c}"))
                .collect();
            print!(
                "{file_prefix}{}:{}:{sev}:{rt}:{found}\u{2192}{display_sug} (\u{00d7}{} also at {})",
                group.first_loc.0,
                group.first_loc.1,
                group.locs.len(),
                rest.join(",")
            );
        }

        // --explain: append context/english on the same line. Sanitize newlines
        // to preserve one-line-per-issue format.
        if explain {
            if let Some(ctx) = &group.context {
                let sanitized = ctx.replace(['\n', '\r'], " ");
                print!(" [{sanitized}]");
            }
            if let Some(eng) = &group.english {
                let sanitized = eng.replace(['\n', '\r'], " ");
                print!(" ({sanitized})");
            }
        }
        println!();
    }
}

/// Print one file's results as header-once TSV. `header_printed` is shared
/// across files so the header appears exactly once per run.
pub(crate) fn render_tabular(r: &FileReport<'_>, explain: bool, header_printed: &mut bool) {
    use std::fmt::Write as FmtWrite;
    use zhtw_mcp::mcp::tools::{
        compress_locations, escape_tsv_field, group_issues, shorten_severity, shorten_type,
    };

    if r.issues.is_empty() {
        return;
    }

    let groups = group_issues(r.issues, explain);
    let file_prefix = display_path_prefix(r.file_arg);

    if !*header_printed {
        if explain {
            println!("found\tsug\ttype\tsev\tn\tloc\texpl");
        } else {
            println!("found\tsug\ttype\tsev\tn\tloc");
        }
        *header_printed = true;
    }

    for ((found, rt, _, sev), group) in &groups {
        // Cannot reuse format_suggestions: each entry is TSV-escaped before
        // joining. Only the delete-sentinel predicate is shared.
        let sug_str = if zhtw_mcp::rules::ruleset::is_delete_suggestion(&group.suggestions) {
            zhtw_mcp::rules::ruleset::DELETE_SUGGESTION.to_string()
        } else {
            group
                .suggestions
                .iter()
                .map(|s| escape_tsv_field(s))
                .collect::<Vec<_>>()
                .join(",")
        };

        // When a file prefix is present, each location must be individually
        // prefixed so consumers can parse "file:L:C,file:L:C" tuples correctly.
        let loc_str = if file_prefix.is_empty() {
            compress_locations(&group.locs)
        } else {
            group
                .locs
                .iter()
                .map(|(l, c)| format!("{file_prefix}{l}:{c}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut line = String::new();
        let _ = write!(
            line,
            "{}\t{sug_str}\t{}\t{}\t{}\t{}",
            escape_tsv_field(found),
            shorten_type(rt),
            shorten_severity(sev),
            group.count,
            escape_tsv_field(&loc_str),
        );
        if explain {
            if let Some(ref expl) = group.explanation {
                let _ = write!(line, "\t{}", escape_tsv_field(expl));
            } else {
                line.push('\t');
            }
        }
        println!("{line}");
    }
}

/// Accumulate one file's results into the run-wide SARIF rule and result sets.
pub(crate) fn collect_sarif(
    r: &FileReport<'_>,
    rules: &mut std::collections::BTreeMap<String, SarifRuleDef>,
    results: &mut Vec<SarifResult>,
) {
    for issue in r.issues {
        let rule_name = issue.rule_type.name();
        let rule_id = format!("zhtw-mcp/{rule_name}");
        let level = match issue.severity {
            zhtw_mcp::rules::ruleset::Severity::Error => "error",
            zhtw_mcp::rules::ruleset::Severity::Warning => "warning",
            zhtw_mcp::rules::ruleset::Severity::Info => "note",
        };

        rules
            .entry(rule_id.clone())
            .or_insert_with(|| SarifRuleDef {
                id: rule_id.clone(),
                short_description: SarifMessage {
                    text: format!("{rule_name} check"),
                },
            });

        results.push(SarifResult {
            rule_id,
            level,
            message: SarifMessage {
                text: format!(
                    "'{}' -> {}",
                    issue.found,
                    format_suggestions(&issue.suggestions)
                ),
            },
            locations: [SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        uri: r.file_arg.to_string(),
                        uri_base_id: "%SRCROOT%",
                    },
                    region: SarifRegion {
                        start_line: issue.line,
                        start_column: issue.col,
                        byte_offset: issue.offset,
                        byte_length: issue.length,
                    },
                },
            }],
        });
    }
}

/// Emit the complete SARIF v2.1.0 document for one lint batch.
///
/// The envelope lives here rather than in the batch: it is five nested
/// wrapper structs and a schema URL, which is rendering, and keeping it here
/// lets every struct below `SarifRuleDef` and `SarifResult` stay private.
pub(crate) fn print_sarif(
    rules: std::collections::BTreeMap<String, SarifRuleDef>,
    results: &[SarifResult],
) -> anyhow::Result<()> {
    let sarif = SarifDocument {
        schema: "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json",
        version: "2.1.0",
        runs: [SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "zhtw-mcp",
                    version: env!("CARGO_PKG_VERSION"),
                    information_uri: SARIF_INFORMATION_URI,
                    rules: rules.into_values().collect(),
                },
            },
            results,
        }],
    };
    println!("{}", serde_json::to_string_pretty(&sarif)?);
    Ok(())
}
