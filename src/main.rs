use std::collections::BTreeSet;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};

// ANSI color helpers for human-format output

/// Whether stderr supports ANSI colors.
fn use_color() -> bool {
    // Respect NO_COLOR env var (https://no-color.org/).
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stderr().is_terminal()
}

struct Colors {
    red: &'static str,
    yellow: &'static str,
    cyan: &'static str,
    dim: &'static str,
    bold: &'static str,
    reset: &'static str,
}

const COLORS_ON: Colors = Colors {
    red: "\x1b[31m",
    yellow: "\x1b[33m",
    cyan: "\x1b[36m",
    dim: "\x1b[2m",
    bold: "\x1b[1m",
    reset: "\x1b[0m",
};

const COLORS_OFF: Colors = Colors {
    red: "",
    yellow: "",
    cyan: "",
    dim: "",
    bold: "",
    reset: "",
};

// Command line
//
// Parsing is a pure function over argv (`parse_args`) and execution is a
// separate dispatch (`run`). They were one 845-line `main` that mixed the two,
// which meant every flag combination could only be tested by spawning the
// binary. Keep them separate: `parse_args` must not touch the filesystem, the
// environment, or the network, so the flag matrix stays unit-testable.

/// A fully parsed command line: global flags plus the selected subcommand.
struct Cli {
    overrides_path: Option<PathBuf>,
    suppressions_path: Option<PathBuf>,
    packs_dir: Option<PathBuf>,
    active_packs: Vec<String>,
    config_path: Option<PathBuf>,
    command: Command,
}

/// The subcommand to run.  Absent subcommand means MCP server over stdio.
enum Command {
    Server,
    Lint(Box<LintArgs>),
    Convert(ConvertArgs),
    Setup(String),
    Pack { cmd: String, arg: Option<String> },
    Tm(TmArgs),
    CacheClear,
}

/// Flags accepted after the `lint` subcommand.  Everything that is not a known
/// flag is a file path, which is why `lint` consumes the rest of the argv.
struct LintArgs {
    files: Vec<String>,
    format: LintFormat,
    max_errors: Option<usize>,
    max_warnings: Option<usize>,
    profile: Option<String>,
    content_type: Option<String>,
    exclude_patterns: Vec<String>,
    fix_mode: Option<zhtw_mcp::fixer::FixMode>,
    dry_run: bool,
    explain: bool,
    relaxed: bool,
    exempt_blockquotes: bool,
    consistency: bool,
    detect_ai: bool,
    detect_translationese: bool,
    /// Emit the composite three-axis scorecard.  Set only by `--detect-style`,
    /// which also flips detect_ai and detect_translationese.
    detect_style: bool,
    translationese_domain: zhtw_mcp::engine::translationese_score::TranslationeseDomain,
    ai_threshold_multiplier: f32,
    baseline_path: Option<PathBuf>,
    update_baseline: bool,
    diff_from: Option<String>,
    #[cfg(feature = "translate")]
    verify: bool,
    telemetry: bool,
}

impl Default for LintArgs {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            format: LintFormat::Human,
            max_errors: None,
            max_warnings: None,
            profile: None,
            content_type: None,
            exclude_patterns: Vec::new(),
            fix_mode: None,
            dry_run: false,
            explain: false,
            relaxed: false,
            exempt_blockquotes: false,
            consistency: false,
            detect_ai: false,
            detect_translationese: false,
            detect_style: false,
            translationese_domain:
                zhtw_mcp::engine::translationese_score::TranslationeseDomain::General,
            ai_threshold_multiplier: 1.0,
            baseline_path: None,
            update_baseline: false,
            diff_from: None,
            #[cfg(feature = "translate")]
            verify: false,
            telemetry: false,
        }
    }
}

#[derive(Default)]
struct ConvertArgs {
    files: Vec<String>,
    content_type: Option<String>,
    #[cfg(feature = "translate")]
    verify: bool,
}

#[derive(Default)]
struct TmArgs {
    cmd: String,
    arg: Option<String>,
    found: Option<String>,
    suggested: Option<String>,
    chose: Option<String>,
    context: Option<String>,
}

/// Read a required path value, keeping each flag's own missing-value message.
fn path_value(value: Option<&String>, missing: &'static str) -> Result<PathBuf> {
    Ok(PathBuf::from(value.context(missing)?))
}

/// Validate a `--content-type` value.
///
/// Shared by `lint` and `convert` so a typo is an error in both. `convert` used
/// to accept anything and fall through to auto-detection, which silently gave
/// extension-based behaviour instead of saying the value was wrong.
fn validated_content_type(value: Option<&String>) -> Result<String> {
    let ct = value.context("--content-type requires a value")?;
    match ct.as_str() {
        // `convert` accepted these two abbreviations before the validator was
        // shared, and `run_convert` still has arms for them. Normalize instead
        // of rejecting, so sharing the validator does not quietly drop an
        // argument that used to work, and so `lint` gains them too.
        "md" => Ok("markdown".to_owned()),
        "yml" => Ok("yaml".to_owned()),
        "plain" | "markdown" | "markdown-scan-code" | "yaml" => Ok(ct.clone()),
        _ => anyhow::bail!(
            "unknown content-type: {ct} (expected 'plain', 'markdown', 'markdown-scan-code', or 'yaml')"
        ),
    }
}

/// Read the optional `low|medium|high` level that may follow `--detect-ai` and
/// `--detect-style`.
///
/// Returns `None` when the next argument is not a level, and the caller must
/// then leave the current multiplier alone: `--detect-ai low --detect-style`
/// has to keep the low threshold, not reset it to the default because the
/// second flag carried no level of its own.
fn detect_threshold(next: Option<&String>) -> Option<f32> {
    match next.map(String::as_str) {
        Some("low") => Some(0.5),
        Some("medium") => Some(1.0),
        Some("high") => Some(1.5),
        _ => None,
    }
}

/// Reject a second subcommand rather than letting one win by dispatch order,
/// which is what the flat-variable version did.
fn claim(current: &Command, name: &str) -> Result<()> {
    match current {
        Command::Server => Ok(()),
        _ => anyhow::bail!("only one subcommand is allowed, found a second: {name}"),
    }
}

/// Parse argv (including argv[0]) into a `Cli`.
///
/// Pure: no filesystem, environment, or network access.  Defaults that need any
/// of those are resolved in `run`.
fn parse_args(args: &[String]) -> Result<Cli> {
    // Usage:
    //   zhtw-mcp                                — run MCP server (default paths)
    //   zhtw-mcp --overrides <path>             — custom overrides JSON path
    //   zhtw-mcp --suppressions <path>          — custom suppressions JSON path
    //   zhtw-mcp --pack <name>                  — activate a rule pack (repeatable)
    //   zhtw-mcp lint <file|--> [--format json|compact]  — lint file(s) or stdin
    //                           [--max-errors N]
    //                           [--profile P] [--detect-ai]
    //                           [--content-type plain|markdown|yaml]
    //   zhtw-mcp setup <host>                   — generate agentic editor integration config
    //   zhtw-mcp pack import <file>             — install a pack
    //   zhtw-mcp pack export <name>             — export a pack
    //   zhtw-mcp pack validate <file>           — validate a pack file
    //   zhtw-mcp pack list                      — list available packs
    let mut cli = Cli {
        overrides_path: None,
        suppressions_path: None,
        packs_dir: None,
        active_packs: Vec::new(),
        config_path: None,
        command: Command::Server,
    };
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--overrides" | "--db" => {
                i += 1;
                cli.overrides_path = Some(path_value(args.get(i), "--overrides requires a path")?);
            }
            "--pack" => {
                i += 1;
                cli.active_packs
                    .push(args.get(i).context("--pack requires a name")?.clone());
            }
            "--packs-dir" => {
                i += 1;
                cli.packs_dir = Some(path_value(args.get(i), "--packs-dir requires a path")?);
            }
            "lint" => {
                claim(&cli.command, "lint")?;
                let (lint, used) = parse_lint(&args[i + 1..])?;
                i += used;
                cli.command = Command::Lint(Box::new(lint));
            }
            "setup" => {
                claim(&cli.command, "setup")?;
                i += 1;
                cli.command = Command::Setup(
                    args.get(i)
                        .context("setup requires a host name")?
                        .to_string(),
                );
            }
            "convert" => {
                claim(&cli.command, "convert")?;
                let (convert, used) = parse_convert(&args[i + 1..])?;
                i += used;
                cli.command = Command::Convert(convert);
            }
            "tm" => {
                claim(&cli.command, "tm")?;
                let (tm, used) = parse_tm(&args[i + 1..])?;
                i += used;
                cli.command = Command::Tm(tm);
            }
            "pack" => {
                claim(&cli.command, "pack")?;
                let (cmd, arg, used) = parse_pack(&args[i + 1..])?;
                i += used;
                cli.command = Command::Pack { cmd, arg };
            }
            "cache" => {
                claim(&cli.command, "cache")?;
                i += parse_cache(&args[i + 1..])?;
                cli.command = Command::CacheClear;
            }
            "--suppressions" => {
                i += 1;
                cli.suppressions_path =
                    Some(path_value(args.get(i), "--suppressions requires a path")?);
            }
            "--config" => {
                i += 1;
                cli.config_path = Some(path_value(args.get(i), "--config requires a path")?);
            }
            "--verbose" => {}
            "--debug" => {}
            _ => {
                anyhow::bail!("unknown argument: {}", args[i]);
            }
        }
        i += 1;
    }

    Ok(cli)
}

/// Parse the arguments after `lint`.
///
/// `lint` consumes the rest of the command line: anything that is not a known
/// flag is a file path, so a global flag written after `lint` becomes a path
/// rather than an error. Returns the arguments consumed, like every other
/// subcommand parser here, so `parse_args` advances its cursor the same way for
/// all of them.
fn parse_lint(rest: &[String]) -> Result<(LintArgs, usize)> {
    let mut lint = LintArgs::default();
    let mut i = 0;

    while i < rest.len() {
        match rest[i].as_str() {
            "--format" => {
                i += 1;
                let fmt = rest.get(i).context("--format requires a value")?;
                lint.format = match fmt.as_str() {
                    "json" => LintFormat::Json,
                    "human" => LintFormat::Human,
                    "sarif" => LintFormat::Sarif,
                    "compact" => LintFormat::Compact,
                    "tabular" => LintFormat::Tabular,
                    _ => anyhow::bail!(
                        "unknown format: {fmt} (expected 'json', 'human', 'sarif', 'compact', or 'tabular')"
                    ),
                };
            }
            "--max-errors" => {
                i += 1;
                lint.max_errors = Some(
                    rest.get(i)
                        .context("--max-errors requires a number")?
                        .parse()
                        .context("--max-errors must be a non-negative integer")?,
                );
            }
            "--max-warnings" => {
                i += 1;
                lint.max_warnings = Some(
                    rest.get(i)
                        .context("--max-warnings requires a number")?
                        .parse()
                        .context("--max-warnings must be a non-negative integer")?,
                );
            }
            "--profile" => {
                i += 1;
                lint.profile = Some(rest.get(i).context("--profile requires a value")?.clone());
            }
            "--relaxed" => {
                lint.relaxed = true;
            }
            "--exempt-blockquotes" => {
                lint.exempt_blockquotes = true;
            }
            "--consistency" => {
                lint.consistency = true;
            }
            "--content-type" => {
                i += 1;
                lint.content_type = Some(validated_content_type(rest.get(i))?);
            }
            "--exclude" => {
                i += 1;
                lint.exclude_patterns
                    .push(rest.get(i).context("--exclude requires a pattern")?.clone());
            }
            "--fix" | "--fix=lexical_safe" => {
                lint.fix_mode = Some(zhtw_mcp::fixer::FixMode::LexicalSafe);
            }
            "--fix=orthographic" => {
                lint.fix_mode = Some(zhtw_mcp::fixer::FixMode::Orthographic);
            }
            "--fix=lexical_contextual" => {
                lint.fix_mode = Some(zhtw_mcp::fixer::FixMode::LexicalContextual);
            }
            arg if arg.starts_with("--fix=") => {
                anyhow::bail!(
                    "unknown fix mode: {} (expected 'orthographic', 'lexical_safe', or 'lexical_contextual')",
                    &arg[6..]
                );
            }
            "--dry-run" => {
                lint.dry_run = true;
            }
            "--explain" => {
                lint.explain = true;
            }
            "--baseline" => {
                i += 1;
                lint.baseline_path =
                    Some(path_value(rest.get(i), "--baseline requires a file path")?);
            }
            "--update-baseline" => {
                lint.update_baseline = true;
            }
            "--diff-from" => {
                i += 1;
                lint.diff_from = Some(
                    rest.get(i)
                        .context("--diff-from requires a git ref")?
                        .clone(),
                );
            }
            "--detect-ai" => {
                lint.detect_ai = true;
                if let Some(mult) = detect_threshold(rest.get(i + 1)) {
                    lint.ai_threshold_multiplier = mult;
                    i += 1;
                }
            }
            "--detect-translationese" => {
                lint.detect_translationese = true;
            }
            "--translationese-domain" => {
                // Per-domain threshold calibration for the
                // translationese score: general | technical |
                // literary | news.
                let next = rest.get(i + 1).context(
                    "--translationese-domain requires a value (general|technical|literary|news)",
                )?;
                let domain =
                    zhtw_mcp::engine::translationese_score::TranslationeseDomain::from_str_strict(
                        next,
                    );
                lint.translationese_domain = domain.with_context(|| {
                    format!(
                        "unknown --translationese-domain value '{next}' (expected: general|technical|literary|news)"
                    )
                })?;
                i += 1;
            }
            "--detect-style" => {
                // Combined shorthand: enable both AI filler and translationese
                // detection. Scores remain orthogonal — reported side by side,
                // never merged.
                lint.detect_ai = true;
                lint.detect_translationese = true;
                lint.detect_style = true;

                // Keep the same optional threshold syntax as --detect-ai.
                if let Some(mult) = detect_threshold(rest.get(i + 1)) {
                    lint.ai_threshold_multiplier = mult;
                    i += 1;
                }
            }
            #[cfg(feature = "translate")]
            "--verify" => {
                lint.verify = true;
            }
            #[cfg(not(feature = "translate"))]
            "--verify" => {
                anyhow::bail!(
                    "--verify requires the 'translate' feature; rebuild with --features translate"
                );
            }
            "--telemetry" => {
                lint.telemetry = true;
            }
            "--verbose" => {}
            "--debug" => {}
            _ => {
                lint.files.push(rest[i].clone());
            }
        }
        i += 1;
    }

    if lint.files.is_empty() {
        anyhow::bail!("lint requires at least one file path or '--' for stdin");
    }
    if lint.detect_style && !matches!(lint.format, LintFormat::Json) {
        anyhow::bail!("--detect-style is only supported with --format json");
    }
    Ok((lint, i))
}

/// Parse the arguments after `convert`, which also consumes the rest of the
/// command line.  With no file arguments it reads stdin.
fn parse_convert(rest: &[String]) -> Result<(ConvertArgs, usize)> {
    let mut convert = ConvertArgs::default();
    let mut i = 0;

    while i < rest.len() {
        match rest[i].as_str() {
            "--content-type" => {
                i += 1;
                convert.content_type = Some(validated_content_type(rest.get(i))?);
            }
            #[cfg(feature = "translate")]
            "--verify" => {
                convert.verify = true;
            }
            #[cfg(not(feature = "translate"))]
            "--verify" => {
                anyhow::bail!(
                    "--verify requires the 'translate' feature; rebuild with --features translate"
                );
            }
            "--" => {
                convert.files.push("--".into());
            }
            arg if arg.starts_with('-') => {
                anyhow::bail!("unknown convert flag: {arg}");
            }
            _ => {
                convert.files.push(rest[i].clone());
            }
        }
        i += 1;
    }
    if convert.files.is_empty() {
        convert.files.push("--".into()); // default: stdin
    }
    Ok((convert, i))
}

/// Parse the arguments after `tm`.  Only `export`, `import`, and `record`
/// consume anything beyond the subcommand name; an unknown subcommand is passed
/// through so `run_tm_cmd` reports it.
fn parse_tm(rest: &[String]) -> Result<(TmArgs, usize)> {
    let mut tm = TmArgs {
        cmd: rest
            .first()
            .context("tm requires a subcommand (list|export|import|clear|record)")?
            .clone(),
        ..TmArgs::default()
    };
    let mut i = 1;

    match tm.cmd.as_str() {
        "export" | "import" => {
            tm.arg = Some(
                rest.get(i)
                    .with_context(|| format!("tm {} requires a file path", tm.cmd))?
                    .clone(),
            );
            i += 1;
        }
        "record" => {
            while i < rest.len() && rest[i].starts_with("--") {
                let flag = rest[i].as_str();
                let slot = match flag {
                    "--found" => &mut tm.found,
                    "--suggested" => &mut tm.suggested,
                    "--chose" => &mut tm.chose,
                    "--context" => &mut tm.context,
                    other => anyhow::bail!("unknown tm record flag: {other}"),
                };
                *slot = Some(
                    rest.get(i + 1)
                        .with_context(|| format!("{flag} requires a value"))?
                        .clone(),
                );
                i += 2;
            }
        }
        _ => {} // list, clear, and anything run_tm_cmd should reject
    }
    Ok((tm, i))
}

/// Parse the arguments after `pack`.  Only `import`, `export`, and `validate`
/// take an argument; `list` does not, and an unknown subcommand is passed
/// through so `run_pack_cmd` reports it.
fn parse_pack(rest: &[String]) -> Result<(String, Option<String>, usize)> {
    let cmd = rest
        .first()
        .context("pack requires a subcommand (import|export|validate|list)")?
        .clone();
    match cmd.as_str() {
        "import" | "export" | "validate" => {
            let arg = rest
                .get(1)
                .with_context(|| format!("pack {cmd} requires an argument"))?
                .clone();
            Ok((cmd, Some(arg), 2))
        }
        _ => Ok((cmd, None, 1)),
    }
}

/// Parse the arguments after `cache`.  `clear` is the only subcommand and it
/// takes nothing, so trailing arguments are a typo worth reporting.
fn parse_cache(rest: &[String]) -> Result<usize> {
    match rest.first().map(String::as_str) {
        Some("clear") => match rest.get(1) {
            Some(extra) => {
                anyhow::bail!("cache clear does not accept additional arguments: {extra}")
            }
            None => Ok(1),
        },
        Some(other) => anyhow::bail!("unknown cache subcommand: {other} (expected 'clear')"),
        None => anyhow::bail!("cache requires a subcommand (clear)"),
    }
}

/// Text failed a gate: too many errors or warnings.  The input was linted
/// successfully; the answer is "no".
const EXIT_GATE: i32 = 1;

/// The tool could not do its job: bad arguments, unreadable config, a file it
/// could not process.  Distinct from [EXIT_GATE] so CI can tell "your prose
/// needs work" from "this run is meaningless".
const EXIT_FAILURE: i32 = 2;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let default_log = if args.iter().any(|a| a == "--debug") {
        "debug"
    } else if args.iter().any(|a| a == "--verbose") {
        "info"
    } else {
        "warn"
    };
    zhtw_mcp::trace::init(default_log);

    // Debug formatting rather than alternate Display: that is what anyhow's own
    // Termination impl used before this function stopped returning Result, so
    // the multi-line "Caused by:" chain users are reading in CI logs stays as
    // it was. The only thing that changed is the exit code.
    let result = parse_args(&args).and_then(run);
    if let Err(e) = result {
        eprintln!("Error: {e:?}");
        process::exit(EXIT_FAILURE);
    }
}

/// Execute a parsed command line.  Everything that reads the environment, the
/// filesystem, or the network lives here rather than in `parse_args`.
fn run(cli: Cli) -> Result<()> {
    let Cli {
        overrides_path,
        suppressions_path,
        packs_dir,
        active_packs,
        config_path,
        command,
    } = cli;
    let packs_dir = packs_dir.unwrap_or_else(zhtw_mcp::rules::store::default_packs_dir);

    match command {
        // Setup subcommand: generate integration config for a host editor.
        Command::Setup(host) => {
            if host == "translation-guide" || host == "translation_guide" {
                return run_translation_guide();
            }
            run_setup(&host)
        }

        Command::CacheClear => {
            let mut cache = zhtw_mcp::rules::judgment_cache::JudgmentCache::open_default();
            let count = cache.len();
            cache.clear();
            cache.flush();
            eprintln!("judgment cache cleared ({count} entries removed)");
            Ok(())
        }

        Command::Convert(convert) => run_convert(
            &convert.files,
            convert.content_type.as_deref(),
            overrides_path.unwrap_or_else(zhtw_mcp::rules::store::default_overrides_path),
            #[cfg(feature = "translate")]
            convert.verify,
        ),

        // TM subcommand: manage translation memory. Respect .zhtw-mcp.toml
        // translation_memory override so `tm record` writes to the same file
        // that `lint` reads.
        Command::Tm(tm) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let project_cfg = match &config_path {
                Some(p) => Some(zhtw_mcp::config::ProjectConfig::from_file(p)?),
                None => zhtw_mcp::config::ProjectConfig::discover(&cwd),
            };
            let tm_path = project_cfg
                .as_ref()
                .and_then(|c| c.translation_memory.as_ref().map(PathBuf::from))
                .unwrap_or_else(|| zhtw_mcp::rules::store::discover_tm_path(&cwd));
            run_tm_cmd(
                &tm.cmd,
                tm.arg.as_deref(),
                &tm_path,
                tm.found.as_deref(),
                tm.suggested.as_deref(),
                tm.chose.as_deref(),
                tm.context.as_deref(),
            )
        }

        // Pack subcommand: manage rule packs.
        Command::Pack { cmd, arg } => run_pack_cmd(&cmd, arg.as_deref(), &packs_dir),

        // Lint subcommand: batch mode supporting multiple files.
        Command::Lint(lint) => {
            run_lint(*lint, overrides_path, config_path, packs_dir, active_packs)
        }

        Command::Server => {
            // Server mode: open the stores, then run MCP over stdio.
            //
            // All three store paths fall back to .zhtw-mcp.toml, the same
            // discovery the tm subcommand uses, so a project can point the
            // server at its own stores without every MCP client passing flags.
            // Wiring only some of them would be worse than wiring none: the
            // server would answer from the project's overrides while recording
            // decisions into a different translation memory than lint reads.
            let cwd = std::env::current_dir().unwrap_or_default();
            let project_cfg = match &config_path {
                Some(p) => Some(zhtw_mcp::config::ProjectConfig::from_file(p)?),
                None => zhtw_mcp::config::ProjectConfig::discover(&cwd),
            };
            let cfg_ref = project_cfg.as_ref();
            let overrides_path = overrides_path
                .or_else(|| cfg_ref.and_then(|c| c.overrides.as_ref().map(PathBuf::from)))
                .unwrap_or_else(zhtw_mcp::rules::store::default_overrides_path);
            let suppressions_path = suppressions_path
                .or_else(|| cfg_ref.and_then(|c| c.suppressions.as_ref().map(PathBuf::from)))
                .unwrap_or_else(zhtw_mcp::rules::store::default_suppressions_path);
            let tm_path = cfg_ref
                .and_then(|c| c.translation_memory.as_ref().map(PathBuf::from))
                .unwrap_or_else(|| zhtw_mcp::rules::store::discover_tm_path(&cwd));
            run_server(
                &overrides_path,
                &suppressions_path,
                &tm_path,
                packs_dir,
                active_packs,
            )
        }
    }
}

/// Merge `lint` flags with `.zhtw-mcp.toml`, then run the batch.  CLI flags win
/// over config values, config values win over defaults.
fn run_lint(
    lint: LintArgs,
    overrides_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    packs_dir: PathBuf,
    mut active_packs: Vec<String>,
) -> Result<()> {
    // Load project config: explicit --config > auto-discover from cwd.
    let project_cfg = match &config_path {
        Some(p) => Some(zhtw_mcp::config::ProjectConfig::from_file(p)?),
        None => {
            let cwd = std::env::current_dir().unwrap_or_default();
            zhtw_mcp::config::ProjectConfig::discover(&cwd)
        }
    };

    let cfg_ref = project_cfg.as_ref();
    let eff_overrides = overrides_path
        .or_else(|| cfg_ref.and_then(|c| c.overrides.as_ref().map(PathBuf::from)))
        .unwrap_or_else(zhtw_mcp::rules::store::default_overrides_path);
    let eff_profile = lint
        .profile
        .as_deref()
        .or_else(|| cfg_ref.and_then(|c| c.profile.as_deref()));
    // CLI --relaxed flag overrides config file relaxed setting.
    let eff_relaxed = lint.relaxed || cfg_ref.and_then(|c| c.relaxed).unwrap_or(false);
    // CLI --exempt-blockquotes flag OR `[markdown] exempt_blockquotes`.
    let eff_exempt_blockquotes = lint.exempt_blockquotes
        || cfg_ref
            .and_then(|c| c.markdown.as_ref())
            .and_then(|m| m.exempt_blockquotes)
            .unwrap_or(false);
    let eff_content_type = lint
        .content_type
        .as_deref()
        .or_else(|| cfg_ref.and_then(|c| c.content_type.as_deref()));
    let eff_max_errors = lint
        .max_errors
        .or_else(|| cfg_ref.and_then(|c| c.max_errors))
        .unwrap_or(0);
    let eff_max_warnings = lint
        .max_warnings
        .or_else(|| cfg_ref.and_then(|c| c.max_warnings));

    // Merge exclude patterns: CLI + config.
    let mut exclude_patterns = lint.exclude_patterns;
    if let Some(cfg_exclude) = cfg_ref.and_then(|c| c.exclude.as_ref()) {
        for pat in cfg_exclude {
            if !exclude_patterns.contains(pat) {
                exclude_patterns.push(pat.clone());
            }
        }
    }

    // Merge packs: CLI + config.
    if let Some(cfg_packs) = cfg_ref.and_then(|c| c.packs.as_ref()) {
        for p in cfg_packs {
            if !active_packs.contains(p) {
                active_packs.push(p.clone());
            }
        }
    }

    // Resolve TM path: config override > auto-discover from cwd.
    let eff_tm_path = cfg_ref
        .and_then(|c| c.translation_memory.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_default();
            zhtw_mcp::rules::store::discover_tm_path(&cwd)
        });

    // Build project glossary from `[glossary]` section.
    let eff_glossary = cfg_ref
        .and_then(|c| c.glossary.as_ref())
        .map(|g| zhtw_mcp::rules::glossary::ProjectGlossary {
            banned: g.banned.clone().unwrap_or_default(),
            preferred: g.preferred.clone().unwrap_or_default(),
            proper_nouns: g.proper_nouns.clone().unwrap_or_default(),
        })
        .unwrap_or_default();

    // ignore_terms is config-only: there is no CLI flag for it, matching the
    // documented field list in docs/cli.md.
    let eff_ignore_terms: Vec<String> = cfg_ref
        .and_then(|c| c.ignore_terms.clone())
        .unwrap_or_default();

    run_lint_batch(&LintBatchParams {
        file_args: &lint.files,
        format: lint.format,
        max_errors: eff_max_errors,
        max_warnings: eff_max_warnings,
        profile_name: eff_profile,
        content_type_override: eff_content_type,
        overrides_path: &eff_overrides,
        packs_dir: &packs_dir,
        active_packs: &active_packs,
        exclude_patterns: &exclude_patterns,
        fix_mode: lint.fix_mode.unwrap_or(zhtw_mcp::fixer::FixMode::None),
        dry_run: lint.dry_run,
        explain: lint.explain,
        baseline_path: lint.baseline_path.as_deref(),
        update_baseline: lint.update_baseline,
        diff_from: lint.diff_from.as_deref(),
        #[cfg(feature = "translate")]
        verify: lint.verify,
        relaxed: eff_relaxed,
        exempt_blockquotes: eff_exempt_blockquotes,
        detect_ai: lint.detect_ai,
        detect_translationese: lint.detect_translationese,
        detect_style: lint.detect_style,
        translationese_domain: lint.translationese_domain,
        ai_threshold_multiplier: lint.ai_threshold_multiplier,
        tm_path: Some(eff_tm_path),
        glossary: eff_glossary,
        ignore_terms: &eff_ignore_terms,
        consistency: lint.consistency,
        telemetry: lint.telemetry,
    })
}

/// Open the stores and serve MCP over stdio.
fn run_server(
    overrides_path: &Path,
    suppressions_path: &Path,
    tm_path: &Path,
    packs_dir: PathBuf,
    active_packs: Vec<String>,
) -> Result<()> {
    let store = zhtw_mcp::rules::store::OverrideStore::open(overrides_path)?;
    let suppression_store = zhtw_mcp::rules::store::SuppressionStore::open(suppressions_path)?;
    let pack_store = zhtw_mcp::rules::store::PackStore::new(packs_dir);

    // Translation memory: the caller resolved the path, from translation_memory
    // in the project config or by walking up from cwd. A missing or unreadable
    // TM degrades to none with a warning, the same as on the lint path, because
    // it is an optional store rather than a precondition.
    let tm_store = match zhtw_mcp::rules::store::TranslationMemoryStore::open(tm_path) {
        Ok(store) => Some(store),
        Err(e) => {
            tracing::warn!(
                "failed to open translation memory at {}: {e}",
                tm_path.display()
            );
            None
        }
    };

    let server = zhtw_mcp::mcp::tools::Server::new(
        store,
        suppression_store,
        pack_store,
        active_packs,
        tm_store,
    )?;

    tracing::info!("zhtw-mcp server starting on stdio");

    // One stdio connection, one server behind one lock: a worker pool per core
    // would be idle threads. The lint pipeline runs on the blocking pool so it
    // never stalls the protocol loop.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(async {
        use rmcp::ServiceExt;
        let service = zhtw_mcp::mcp::sdk::SdkServer::new(server);
        let transport = zhtw_mcp::mcp::transport::stdio(service.lifecycle());
        let running = match service.serve(transport).await {
            Ok(running) => running,
            // A client that closed the pipe before the handshake is a client
            // that went away, not a failure to report: the hand-rolled
            // transport returned cleanly on EOF and supervisors still probe
            // this binary by spawning it and closing stdin.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(reason)) => {
                tracing::info!("client disconnected before initialize: {reason}");
                return Ok(());
            }
            // A pre-handshake request the SDK answered and then declined to
            // continue from, which in practice is a `server/discover` missing
            // the per-request metadata its revision requires. The client has
            // its error; ending quietly is the whole of the outcome.
            Err(rmcp::service::ServerInitializeError::ExpectedInitializeRequest(message)) => {
                tracing::warn!("client ended the session before initialize: {message:?}");
                return Ok(());
            }
            // The client asked for a protocol revision this server does not
            // serve and was told so, with the list it can choose from. The
            // handshake failing ends the session, but a negotiation that
            // reached a definite answer is not a crash to report as one.
            Err(rmcp::service::ServerInitializeError::InitializeFailed(error)) => {
                tracing::warn!("handshake refused: {error:?}");
                return Ok(());
            }
            Err(e) => return Err(anyhow::Error::from(e)),
        };
        running.waiting().await?;
        Ok::<(), anyhow::Error>(())
    });

    // End of input is bounded on purpose: the transport waits a fixed while
    // for responses still owed and then reports EOF regardless. Dropping the
    // runtime here would undo that, because a runtime waits for its blocking
    // tasks with no deadline and the lint runs on that pool. A scan wedged
    // past the drain would hold the process open indefinitely, having already
    // been given up on. Shutting down with a deadline keeps the exit bounded;
    // whatever the scan still held is lost either way, and the judgment cache
    // is flushed on the `exit` path rather than here.
    runtime.shutdown_timeout(zhtw_mcp::mcp::transport::BLOCKING_SHUTDOWN_GRACE);
    outcome
}

// Lint subcommand

#[derive(Clone, Copy)]
enum LintFormat {
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
    fn report_owns_stdout(self) -> bool {
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
struct CliFileOutput {
    file: String,
    detected_script: String,
    issues: Vec<zhtw_mcp::rules::ruleset::Issue>,
    total: usize,
    errors: usize,
    warnings: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    tm_suppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fixes_applied: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fixes_skipped: Option<usize>,
    /// Subset of fixes_skipped the fixer judged on the issue's own merits.
    /// fixes_skipped also counts issues that were never in scope for the tier,
    /// overlapped an earlier fix, or landed in an excluded region.
    #[serde(skip_serializing_if = "Option::is_none")]
    fixes_declined: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_signature: Option<zhtw_mcp::engine::ai_score::AiSignatureReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    translationese_signature: Option<zhtw_mcp::engine::translationese_score::TranslationeseReport>,
    /// Composite style scorecard.  Three orthogonal axes, never
    /// collapsed into a single number.  Present only when --detect-style
    /// is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    style_scorecard: Option<zhtw_mcp::engine::style_score::StyleScorecard>,
    /// Document-wide consistency report (35.1).  Present only when
    /// --consistency is set AND mixed regional usage is detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    consistency: Option<zhtw_mcp::engine::consistency::ConsistencyReport>,
}

#[derive(serde::Serialize)]
struct SarifDocument<'a> {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: [SarifRun<'a>; 1],
}

#[derive(serde::Serialize)]
struct SarifRun<'a> {
    tool: SarifTool,
    results: &'a [SarifResult],
}

#[derive(serde::Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifDriver {
    name: &'static str,
    version: &'static str,
    information_uri: &'static str,
    rules: Vec<SarifRuleDef>,
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
struct SarifRuleDef {
    id: String,
    short_description: SarifMessage,
}

#[derive(serde::Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
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

struct LintBatchParams<'a> {
    file_args: &'a [String],
    format: LintFormat,
    max_errors: usize,
    max_warnings: Option<usize>,
    profile_name: Option<&'a str>,
    content_type_override: Option<&'a str>,
    overrides_path: &'a Path,
    packs_dir: &'a Path,
    active_packs: &'a [String],
    exclude_patterns: &'a [String],
    fix_mode: zhtw_mcp::fixer::FixMode,
    dry_run: bool,
    explain: bool,
    baseline_path: Option<&'a Path>,
    update_baseline: bool,
    diff_from: Option<&'a str>,
    #[cfg(feature = "translate")]
    verify: bool,
    relaxed: bool,
    exempt_blockquotes: bool,
    detect_ai: bool,
    detect_translationese: bool,
    /// Emit composite three-axis style scorecard alongside the per-axis
    /// ai_signature / translationese_signature reports.  Set only by
    /// `--detect-style` (which also flips detect_ai +
    /// detect_translationese).
    detect_style: bool,
    translationese_domain: zhtw_mcp::engine::translationese_score::TranslationeseDomain,
    ai_threshold_multiplier: f32,
    tm_path: Option<PathBuf>,
    /// Project glossary (`[glossary]` section in `.zhtw-mcp.toml`).
    /// Applied as a post-scan step: `proper_nouns` suppress matching
    /// issues, `banned` injects synthetic Error issues for any
    /// occurrence the embedded ruleset missed.
    glossary: zhtw_mcp::rules::glossary::ProjectGlossary,
    /// Terms the project has declared uninteresting (`ignore_terms` in
    /// `.zhtw-mcp.toml`).  Still reported, but downgraded to Info so they
    /// stop failing the error and warning gates.  Same semantics as the
    /// MCP tool's `ignore_terms` argument.
    ignore_terms: &'a [String],
    /// When true, append a `consistency` block to JSON output (35.1):
    /// per-equivalence-class diagnostic when both the calque and the
    /// canonical TW form appear in the same document.
    consistency: bool,
    telemetry: bool,
}

/// Render a file argument as a `path:` prefix relative to the current
/// directory, or empty for stdin. Shared by the compact and tabular
/// formatters, which had byte-identical copies of this.
fn display_path_prefix(file_arg: &str) -> String {
    if file_arg == "--" {
        return String::new();
    }
    let display_path = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            Path::new(file_arg)
                .strip_prefix(&cwd)
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
struct FileReport<'a> {
    file_arg: &'a str,
    detected_script: &'a str,
    issues: &'a [zhtw_mcp::rules::ruleset::Issue],
    error_count: usize,
    warning_count: usize,
    tm_suppressed: usize,
    fixes_applied: Option<usize>,
    fixes_skipped: Option<usize>,
    fixes_declined: Option<usize>,
    ai_signature: Option<&'a zhtw_mcp::engine::ai_score::AiSignatureReport>,
    translationese_signature:
        Option<&'a zhtw_mcp::engine::translationese_score::TranslationeseReport>,
    /// Text the consistency report should run against: post-fix when fixes
    /// were written, original otherwise.
    consistency_text: &'a str,
    text_char_count: usize,
    multi: bool,
}

/// Build the JSON result object for one file.
fn render_json(r: &FileReport<'_>, params: &LintBatchParams<'_>) -> CliFileOutput {
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
                    &params.glossary,
                )
            })
            .filter(|c| !c.is_empty()),
    }
}

/// Print one file's results in the default human format, to stderr.
fn render_human(r: &FileReport<'_>, params: &LintBatchParams<'_>, c: &Colors) {
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
fn render_compact(r: &FileReport<'_>, explain: bool) {
    use std::collections::HashMap;

    type CompactKey<'a> = (&'a str, &'a str, String, &'a str);
    struct CompactGroup {
        first_loc: (usize, usize),
        locs: Vec<(usize, usize)>,
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
        let (found, rt, sug_key, sev) = key;
        let group = &groups[key];
        // Render suggestion: first entry + count of alternatives.
        let parts: Vec<&str> = sug_key.split('|').collect();
        let display_sug = if parts.len() <= 1 {
            parts.first().copied().unwrap_or("?").to_string()
        } else {
            format!("{}+{}", parts[0], parts.len() - 1)
        };
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
                let sanitized = ctx.replace('\n', " ");
                print!(" [{sanitized}]");
            }
            if let Some(eng) = &group.english {
                print!(" ({eng})");
            }
        }
        println!();
    }
}

/// Print one file's results as header-once TSV. `header_printed` is shared
/// across files so the header appears exactly once per run.
fn render_tabular(r: &FileReport<'_>, explain: bool, header_printed: &mut bool) {
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
fn collect_sarif(
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

/// Everything a lint batch builds once and then reuses for every file.
struct LintSetup {
    cfg: zhtw_mcp::rules::ruleset::ProfileConfig,
    scanner: zhtw_mcp::engine::scan::Scanner,
    ruleset_hash: String,
    /// Built on first Simplified input rather than during setup.  Its
    /// Aho-Corasick over ST_PHRASES dominated startup, and a zh-TW linter
    /// reading zh-TW never needs it: building it eagerly cost 183 ms per
    /// invocation against 44 ms lazily, measured interleaved on a one-line
    /// file with an empty cache.  OnceLock rather than a plain field so the
    /// rayon batch path can share one converter across threads.
    ///
    /// Built first thing in a fresh process the same call measures ~90 ms, but
    /// built here, after the ruleset and scanner have already warmed the
    /// allocator, Simplified input is not measurably slower than Traditional.
    /// Quote the end-to-end numbers rather than that isolated one.
    s2t: std::sync::OnceLock<zhtw_mcp::engine::s2t::S2TConverter>,
    tm_store: Option<zhtw_mcp::rules::store::TranslationMemoryStore>,
    scan_cache: Option<std::sync::Mutex<zhtw_mcp::cache::ScanCache>>,
}

/// Convert `text` when it reads as Simplified, building the converter on the
/// first such file.  Returns whether a conversion happened, which the caller
/// reports as the `+s2t` label.
fn s2t_convert_if_simplified(
    s2t: &std::sync::OnceLock<zhtw_mcp::engine::s2t::S2TConverter>,
    text: &mut String,
) -> bool {
    use zhtw_mcp::engine::s2t::S2TConverter;
    use zhtw_mcp::engine::zhtype::{detect_chinese_type, ChineseType};

    if detect_chinese_type(text) != ChineseType::Simplified {
        return false;
    }
    *text = s2t.get_or_init(S2TConverter::new).convert(text);
    true
}

/// Resolve flags and config into the scanner, stores, and cache the batch
/// runs against.  Split out of `run_lint_batch` because none of it depends
/// on the files being linted: it is pure setup, and reviewing it does not
/// require holding the per-file loop in your head.
fn build_lint_setup(
    params: &LintBatchParams<'_>,
    profile: zhtw_mcp::rules::ruleset::Profile,
) -> Result<LintSetup> {
    // Effective config: profile base plus capability flags.
    let mut cfg = profile.config();
    if params.relaxed {
        cfg = cfg.with_relaxed();
    }
    if params.exempt_blockquotes {
        cfg = cfg.with_exempt_blockquotes(true);
    }
    if params.detect_ai {
        cfg.ai_filler_detection = true;
        cfg.ai_semantic_safety = true;
        cfg.ai_density_detection = true;
        cfg.ai_structural_patterns = true;
        cfg.ai_threshold_multiplier = params.ai_threshold_multiplier;
    }
    if params.detect_translationese {
        cfg.translationese_detection = true;
    }
    cfg.translationese_domain = params.translationese_domain;

    // Build scanner once for all files, merging overrides + active packs.
    let ruleset = zhtw_mcp::rules::loader::load_embedded_ruleset()?;
    let store = zhtw_mcp::rules::store::OverrideStore::open(params.overrides_path)?;
    let pack_store = zhtw_mcp::rules::store::PackStore::new(params.packs_dir.to_path_buf());

    let (spelling_rules, case_rules) = zhtw_mcp::rules::store::build_merged_rules(
        &ruleset.spelling_rules,
        &ruleset.case_rules,
        &store,
        &pack_store,
        params.active_packs,
    );
    let ruleset_hash = zhtw_mcp::rules::loader::compute_ruleset_hash(&spelling_rules, &case_rules);
    let filter = zhtw_mcp::engine::scan::ProfileFilter::from_config(&cfg);
    let scanner =
        zhtw_mcp::engine::scan::Scanner::new_filtered(spelling_rules, case_rules, &filter);

    // Open translation memory (if path provided and file exists/creatable).
    let tm_store = params.tm_path.as_ref().and_then(|p| {
        zhtw_mcp::rules::store::TranslationMemoryStore::open(p)
            .map_err(|e| tracing::warn!("failed to open TM at {}: {e}", p.display()))
            .ok()
    });

    // Scan cache: skip re-scanning unchanged files (lint-only, no fix).
    // Disabled when --verify is active (calibrate_issues needs the full text).
    // Wrapped in Mutex for rayon parallel scanning.
    let use_cache = params.fix_mode == zhtw_mcp::fixer::FixMode::None && {
        #[cfg(feature = "translate")]
        {
            !params.verify
        }
        #[cfg(not(feature = "translate"))]
        {
            true
        }
    };
    let scan_cache =
        use_cache.then(|| std::sync::Mutex::new(zhtw_mcp::cache::ScanCache::open_default()));

    Ok(LintSetup {
        cfg,
        scanner,
        ruleset_hash,
        s2t: std::sync::OnceLock::new(),
        tm_store,
        scan_cache,
    })
}

/// Running counts across every file in one lint batch.  Grouped because
/// the six of them are always read and written together; six loose
/// counters in a 700-line function is how one of them gets missed.
#[derive(Default)]
struct LintTotals {
    errors: usize,
    warnings: usize,
    deterministic: usize,
    heuristic: usize,
    llm_judged: usize,
    unresolved: usize,
}

impl LintTotals {
    fn report_telemetry(&self, file_count: usize) {
        eprintln!(
            "[telemetry] files={} total_issues={} errors={} warnings={}",
            file_count,
            self.errors + self.warnings,
            self.errors,
            self.warnings,
        );
        eprintln!(
            "[telemetry] resolution: deterministic={} heuristic={} llm_judged={} unresolved={}",
            self.deterministic, self.heuristic, self.llm_judged, self.unresolved,
        );
    }
}

fn run_lint_batch(params: &LintBatchParams<'_>) -> Result<()> {
    let c = if use_color() { &COLORS_ON } else { &COLORS_OFF };

    let profile = match params.profile_name {
        None => zhtw_mcp::rules::ruleset::Profile::Base,
        Some(s) => zhtw_mcp::rules::ruleset::Profile::from_str_strict(s)
            .ok_or_else(|| anyhow::anyhow!("unknown profile: {s} (expected 'base' or 'strict')"))?,
    };

    let setup = build_lint_setup(params, profile)?;
    let LintSetup {
        cfg,
        ref scanner,
        ref ruleset_hash,
        ref s2t,
        ref scan_cache,
        ..
    } = setup;

    // --diff-from: resolve changed files via git, use as file args.
    let diff_files: Vec<String>;
    let file_args = if let Some(git_ref) = params.diff_from {
        diff_files = resolve_diff_files(git_ref)?;
        &diff_files
    } else {
        params.file_args
    };

    // Resolve directories into individual files; de-duplicate and sort.
    let resolved = resolve_file_args(file_args, params.exclude_patterns)?;
    let multi = resolved.len() > 1;
    let mut state = BatchState {
        // Load baseline if provided.
        baseline: params
            .baseline_path
            .map(zhtw_mcp::baseline::Baseline::load)
            .transpose()?
            .unwrap_or_default(),
        ..Default::default()
    };

    /// Maximum file size for CLI lint mode (16 MiB).
    const MAX_CLI_FILE_BYTES: u64 = 16 * 1024 * 1024;

    // Phase 1: Read + S2T + cache check + scan.
    //
    // This closure is shared between sequential and parallel (rayon) paths. It
    // captures only &-refs to immutable state plus the Mutex-wrapped cache,
    // making it Fn + Send + Sync.
    let fix_mode_str = format!("{:?}", params.fix_mode);
    let scan_file = |file_arg: &str| -> ScanResult {
        let content_type = match params.content_type_override {
            Some("markdown") => zhtw_mcp::engine::scan::ContentType::Markdown,
            Some("markdown-scan-code") => zhtw_mcp::engine::scan::ContentType::MarkdownScanCode,
            Some("yaml") => zhtw_mcp::engine::scan::ContentType::Yaml,
            Some("plain") => zhtw_mcp::engine::scan::ContentType::Plain,
            Some(_) | None => {
                let lower = file_arg.to_ascii_lowercase();
                if lower.ends_with(".md") || lower.ends_with(".markdown") {
                    zhtw_mcp::engine::scan::ContentType::Markdown
                } else if lower.ends_with(".yml") || lower.ends_with(".yaml") {
                    zhtw_mcp::engine::scan::ContentType::Yaml
                } else {
                    zhtw_mcp::engine::scan::ContentType::Plain
                }
            }
        };

        let cache_params = zhtw_mcp::cache::ScanParams {
            ruleset_hash: ruleset_hash.clone(),

            // The whole effective config, not `profile.name()`. The name is the
            // profile the user asked for; the scanner is built from this
            // struct, and flags such as --relaxed change it without changing
            // the name. Keying on the name let a --relaxed run answer for a
            // strict one and vice versa, so a strict gate could report clean.
            // Debug covers every field, so a new one cannot be forgotten here.
            profile: format!("{cfg:?}"),
            content_type: format!("{content_type:?}"),
            fix_mode: fix_mode_str.clone(),
            detect_ai: params.detect_ai,
            detect_translationese: cfg.translationese_detection,
            translationese_domain: cfg.translationese_domain.name().to_owned(),
            ai_threshold: format!("{:.1}", params.ai_threshold_multiplier),
            exempt_blockquotes: cfg.exempt_blockquotes,
            engine_version: format!(
                "{}+{}",
                env!("CARGO_PKG_VERSION"),
                env!("ZHTW_ENGINE_FINGERPRINT")
            ),
        };

        // Open file via fd, stat from the fd (TOCTOU-safe). Check cache BEFORE
        // reading — fast path avoids file I/O entirely.
        if file_arg != "--" {
            let file =
                std::fs::File::open(file_arg).with_context(|| format!("open file: {file_arg}"))?;
            let meta = file
                .metadata()
                .with_context(|| format!("stat file: {file_arg}"))?;
            anyhow::ensure!(
                meta.len() <= MAX_CLI_FILE_BYTES,
                "{file_arg}: file too large ({} bytes, limit {MAX_CLI_FILE_BYTES})",
                meta.len()
            );

            // Fast-path: check mtime+size before reading the file.
            let fast_hit = scan_cache.as_ref().and_then(|mtx| {
                let mut c = mtx.lock().ok()?;
                let mtime = zhtw_mcp::cache::mtime_secs(&meta);
                c.check_fast(file_arg, mtime, meta.len(), &cache_params)
                    .into_hit()
            });

            // Glossary banned-term injection and the consistency report both
            // scan the original text buffer; the fast path can only
            // short-circuit when neither feature needs it. Same story for
            // fix/SC/verify.
            let need_text_post_scan = params.fix_mode != zhtw_mcp::fixer::FixMode::None
                || !params.glossary.is_empty()
                || params.consistency
                || {
                    #[cfg(feature = "translate")]
                    {
                        params.verify
                    }
                    #[cfg(not(feature = "translate"))]
                    {
                        false
                    }
                };
            if let Some(hit) = fast_hit {
                if !hit.input_was_sc && !need_text_post_scan {
                    // Cache hit AND no later phase needs the text: skip file
                    // read and scan.
                    return Ok((
                        String::new(),
                        false,
                        hit.text_char_count,
                        hit.output,
                        content_type,
                    ));
                }

                // SC files need the text for S2T write-back; glossary /
                // consistency / fix / verify need the original buffer. Fall
                // through to the slow path so we read the file and reuse the
                // cached scan output below.
            }

            // Slow path: read file from the same fd.
            let mut text = String::with_capacity(meta.len() as usize);
            std::io::BufReader::new(file)
                .read_to_string(&mut text)
                .with_context(|| format!("read file: {file_arg}"))?;

            let input_was_sc = s2t_convert_if_simplified(s2t, &mut text);
            let text_char_count = text.chars().count();

            // Slow-path cache: check content hash (mtime missed but content may
            // be unchanged, e.g. after `touch`).
            let content_hit = scan_cache.as_ref().and_then(|mtx| {
                let mut c = mtx.lock().ok()?;
                c.check_content(file_arg, text.as_bytes(), &cache_params)
            });
            let output = match content_hit {
                Some(hit) => hit.output,
                None => {
                    let o = scanner.scan_for_content_type_with_config(&text, content_type, cfg);
                    if let Some(Ok(mut c)) = scan_cache.as_ref().map(|mtx| mtx.lock()) {
                        let mtime = zhtw_mcp::cache::mtime_secs(&meta);
                        c.put(
                            file_arg,
                            text.as_bytes(),
                            mtime,
                            meta.len(),
                            &cache_params,
                            o.clone(),
                            input_was_sc,
                            text_char_count,
                        );
                    }
                    o
                }
            };

            // Drop text eagerly when not needed for fix/write-back/verify to
            // avoid accumulating all files' text in parallel scans. SC input
            // additionally needs it for the S2T write-back.
            let need_text = input_was_sc || need_text_post_scan;
            if !need_text {
                text = String::new();
            }

            return Ok((text, input_was_sc, text_char_count, output, content_type));
        }

        // stdin path.
        let mut text = String::new();
        std::io::stdin()
            .take(MAX_CLI_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .context("read stdin")?;
        anyhow::ensure!(
            text.len() as u64 <= MAX_CLI_FILE_BYTES,
            "stdin input exceeds {MAX_CLI_FILE_BYTES} byte limit"
        );

        let input_was_sc = s2t_convert_if_simplified(s2t, &mut text);
        let text_char_count = text.chars().count();
        let output = scanner.scan_for_content_type_with_config(&text, content_type, cfg);

        Ok((text, input_was_sc, text_char_count, output, content_type))
    };

    // Parallel scan when multiple files and no stdin pipe. Rayon parallelism
    // gives N/cores speedup on multi-file lint.
    let has_stdin = resolved.iter().any(|f| f == "--");
    let scan_results: Vec<ScanResult> = if resolved.len() > 1 && !has_stdin {
        use rayon::prelude::*;
        resolved.par_iter().map(|f| scan_file(f)).collect()
    } else {
        resolved.iter().map(|f| scan_file(f)).collect()
    };

    // Phase 2: Fix + report (always sequential for ordered output).
    let ctx = FileCtx {
        params,
        colors: c,
        setup: &setup,
        profile,
        multi,
    };
    for (file_arg, scan_result) in resolved.iter().zip(scan_results) {
        // A file that cannot be read is reported and skipped, not fatal. One
        // latin-1 document in a directory used to abort the whole run and
        // discard the findings for every file already processed, which in JSON
        // mode meant empty output after doing all the work.
        if let Err(e) = process_scanned_file(&ctx, file_arg, scan_result, &mut state) {
            // No file prefix: every error on this path already carries the path
            // in its context, and prefixing printed it twice. Alternate Display
            // keeps one file to one line, which is what the rest of the
            // per-file output does.
            eprintln!("{}{:#}{}", c.bold, e, c.reset);
            state.failed_files += 1;
        }
    }

    // Multi-file JSON: emit array of per-file results.
    if multi && matches!(params.format, LintFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&state.file_results)?);
    }

    // --update-baseline: save the baseline file.
    if params.update_baseline {
        let bl_path = params
            .baseline_path
            .context("--update-baseline requires --baseline <file>")?;
        state.baseline.save(bl_path)?;
        eprintln!(
            "{}Baseline updated:{} {} fingerprint(s) in {}",
            c.dim,
            c.reset,
            state.baseline.len(),
            bl_path.display()
        );
    }

    // Report baseline summary if filtering was active.
    if params.baseline_path.is_some() && !params.update_baseline && state.baseline_count > 0 {
        eprintln!(
            "{}{} baseline issue(s) suppressed.{}",
            c.dim, state.baseline_count, c.reset
        );
    }

    // SARIF: emit the complete SARIF v2.1.0 document.
    if matches!(params.format, LintFormat::Sarif) {
        let sarif = SarifDocument {
            schema: "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json",
            version: "2.1.0",
            runs: [SarifRun {
                tool: SarifTool {
                    driver: SarifDriver {
                        name: "zhtw-mcp",
                        version: env!("CARGO_PKG_VERSION"),
                        information_uri: SARIF_INFORMATION_URI,
                        rules: state.sarif_rules.into_values().collect(),
                    },
                },
                results: &state.sarif_results,
            }],
        };
        println!("{}", serde_json::to_string_pretty(&sarif)?);
    }

    // Flush scan cache before potential process::exit (which skips Drop).
    if let Some(ref cache_mtx) = scan_cache {
        if let Ok(mut c) = cache_mtx.lock() {
            c.flush();
        }
    }

    // Print telemetry summary to stderr when --telemetry is set.
    if params.telemetry {
        state.totals.report_telemetry(resolved.len());
    }

    // Exit codes are a contract with CI (see docs/cli.md): 1 means the text
    // failed a gate, 2 means the tool could not do its job. A skipped file
    // outranks a gate result, because the gate was computed over an incomplete
    // set and a clean verdict would be a lie.
    if state.failed_files > 0 {
        eprintln!(
            "{}{} file(s) could not be processed{}",
            c.dim, state.failed_files, c.reset
        );
        process::exit(EXIT_FAILURE);
    }

    let errors_exceeded = state.totals.errors > params.max_errors;
    let warnings_exceeded = params
        .max_warnings
        .is_some_and(|limit| state.totals.warnings > limit);
    if errors_exceeded || warnings_exceeded {
        process::exit(EXIT_GATE);
    }

    Ok(())
}

/// One file after phase 1: (raw text, was-SC input, char count, scan
/// output, content type).  Aliased so the tuple slot ordering has a
/// single source of truth.
type ScanResult = Result<(
    String,
    bool,
    usize,
    zhtw_mcp::engine::scan::ScanOutput,
    zhtw_mcp::engine::scan::ContentType,
)>;

/// Immutable context shared by every file in a lint batch.
struct FileCtx<'a> {
    params: &'a LintBatchParams<'a>,
    colors: &'a Colors,
    setup: &'a LintSetup,
    profile: zhtw_mcp::rules::ruleset::Profile,
    multi: bool,
}

/// Everything the per-file pass accumulates and the batch drains after
/// the loop.  These move together, so they live together.
#[derive(Default)]
struct BatchState {
    totals: LintTotals,
    file_results: Vec<CliFileOutput>,
    sarif_results: Vec<SarifResult>,
    sarif_rules: std::collections::BTreeMap<String, SarifRuleDef>,
    baseline: zhtw_mcp::baseline::Baseline,
    baseline_count: usize,
    tabular_header_printed: bool,
    /// Files that could not be processed at all: unreadable, oversized, or not
    /// UTF-8.  Counted rather than propagated, so one bad file in a directory
    /// does not throw away the findings for every other file.
    failed_files: usize,
}

/// What `emit_fix_result` left behind for the phases that follow it.
struct FixEmission<'a> {
    /// The buffer every later phase reads: the fixer's output when it ran, the
    /// original otherwise.
    text: &'a str,
    /// True when the document on disk (or on stdout) now differs from the
    /// input, so reporting has to run against the rewritten text.
    wrote_changes: bool,
}

/// Write the fixed document and report what happened to it.
///
/// Split out of `process_scanned_file` because the two halves are easy to get
/// out of step: stdout carries the document for a stdin filter and the report
/// for every machine format, while status lines always belong on stderr.
/// Keeping the decision in one place means there is one answer to "where does
/// this text go", not one per branch.
fn emit_fix_result<'a>(
    file_arg: &str,
    text: &'a str,
    fix_result: Option<&'a zhtw_mcp::fixer::FixResult>,
    input_was_sc: bool,
    params: &LintBatchParams<'_>,
    c: &Colors,
) -> Result<FixEmission<'a>> {
    // Write fixed text (unless --dry-run). Text is written when either S2T
    // conversion was applied or ruleset fixes were made.
    let fix_applied = fix_result.map_or(0, |f| f.applied);
    let fix_declined = fix_result.map_or(0, |f| f.declined);
    let has_text_changes = input_was_sc || fix_applied > 0;

    // Declined fixes are the only signal that --fix looked at an issue and
    // chose not to rewrite it. Without this the count reaches JSON consumers
    // only, and a file where every issue is declined prints no fix line at all.
    //
    // Reports f.declined, not f.skipped: the latter also counts issues that
    // were never in scope, so "--fix=orthographic" on ordinary prose would
    // report every cross-strait term as declined. Deliberately does not name a
    // tier that would apply them either: declines come from several gates
    // (multiple suggestions, anchor rejection, tier-2 suppression, editorial
    // confidence) and only some are tier-liftable.
    let declined = if fix_declined > 0 {
        format!(", {}{fix_declined} declined{}", c.dim, c.reset)
    } else {
        String::new()
    };

    // The buffer every later phase reads: the fixer's output when it ran, the
    // original otherwise.
    let current_text = fix_result.map_or(text, |f| f.text.as_str());

    // stdin with --fix is a filter, so stdout carries the document whether or
    // not anything changed. Gating this on has_text_changes made "lint -- --fix
    // > out.md" emit nothing for a clean document, which truncates the user's
    // content on the one input that has no copy on disk to fall back to. A dry
    // run still emits nothing: it reports what would happen and leaves the text
    // alone.
    //
    // S2T conversion counts too, with or without --fix. It rewrites the
    // document exactly as a fix does, and the file branch below writes it back
    // unconditionally, so withholding it on stdin loses the converted text with
    // nothing on disk to recover it.
    //
    // Human format only, because stdout can carry one product. Every other
    // format puts its report there, and a document printed ahead of it made
    // "--fix --format json" unparseable. That combination asks for the report,
    // so the report is what stdout gets; the fixed text is reachable by
    // rerunning without --format, or by fixing a file instead.
    let stdin_emits_document =
        file_arg == "--" && (fix_result.is_some() || input_was_sc) && !params.dry_run;
    if stdin_emits_document && !params.format.report_owns_stdout() {
        print!("{}", current_text);
    } else if stdin_emits_document {
        // Say what was dropped. Silence here is how "--fix --format compact"
        // over a pipe emptied a document with nothing on either stream and an
        // exit code of 0: compact and tabular print nothing at all for a clean
        // file, so the discard was indistinguishable from success.
        eprintln!(
            "{}--{}: rewritten text not emitted: --format owns stdout; \
             rerun without --format, or process a file",
            c.bold, c.reset
        );
    }

    // A dry run computes the fixes but emits nothing, so reporting has to stay
    // on the text the user still has.
    let wrote_changes = has_text_changes && !params.dry_run;
    if has_text_changes {
        let s2t_label = if input_was_sc && fix_applied == 0 {
            " (S2T only)"
        } else {
            ""
        };
        if params.dry_run {
            eprintln!(
                "{}{}{}: {} fix(es) would be applied{s2t_label}{declined} {}(dry run){}",
                c.bold, file_arg, c.reset, fix_applied, c.dim, c.reset
            );
        } else if file_arg == "--" {
            // The document already went to stdout above, once, for every stdin
            // path that rewrites it rather than only the one where --fix
            // changed something.
            //
            // Unconditional, like the file branch. Gating it on a nonzero
            // decline count meant stdin reported a fix only when something was
            // also turned down, which is the opposite of what a file does.
            // stdout stays reserved for the document.
            eprintln!(
                "{}--{}: {} fix(es) applied{s2t_label}{declined}",
                c.bold, c.reset, fix_applied
            );
        } else {
            // Atomic write: tempfile + rename in the same directory. Worth the
            // rename semantics here, unlike the baseline: this is the user's
            // source file, and a torn write loses their content rather than a
            // regenerable artifact.
            let file_path = Path::new(file_arg);
            let parent = file_path.parent().unwrap_or(Path::new("."));
            let mut tmp = tempfile::NamedTempFile::new_in(parent)
                .with_context(|| format!("{file_arg}: create tempfile in {}", parent.display()))?;
            std::io::Write::write_all(&mut tmp, current_text.as_bytes())
                .with_context(|| format!("write tempfile for {file_arg}"))?;

            // A temp file is created 0600. Carry over the mode of the file
            // being replaced, or --fix silently turns every source file it
            // touches into 0600 and git reports a mode change on each one. The
            // file was just read, so metadata is expected to succeed; a
            // cosmetic mode bit is not worth failing the write over.
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(file_path) {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o7777;
                let _ = tmp
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(mode));
            }
            tmp.persist(file_path)
                .with_context(|| format!("rename tempfile to {file_arg}"))?;
            eprintln!(
                "{}{}{}: {} fix(es) applied{s2t_label}{declined}",
                c.bold, file_arg, c.reset, fix_applied
            );
        }
    } else if fix_declined > 0 {
        // --fix ran but rewrote nothing. Say so, or the run is
        // indistinguishable from one where --fix was never passed.
        let dry = if params.dry_run {
            format!(" {}(dry run){}", c.dim, c.reset)
        } else {
            String::new()
        };
        eprintln!(
            "{}{}{}: no fixes applied{declined}{dry}",
            c.bold, file_arg, c.reset
        );
    }

    Ok(FixEmission {
        text: current_text,
        wrote_changes,
    })
}

/// Fix, rescan, verify, and report one already-scanned file.
///
/// Split out of `run_lint_batch` so the per-file pipeline can be read
/// without the batch setup and the phase-1 parallel scan around it.
fn process_scanned_file(
    ctx: &FileCtx<'_>,
    file_arg: &str,
    scan_result: ScanResult,
    state: &mut BatchState,
) -> Result<()> {
    let params = ctx.params;
    let c = ctx.colors;
    let scanner = &ctx.setup.scanner;
    let cfg = ctx.setup.cfg;
    let tm_store = &ctx.setup.tm_store;
    let profile = ctx.profile;
    let multi = ctx.multi;

    let (text, input_was_sc, text_char_count, output, content_type) = scan_result?;

    let detected_script = if input_was_sc {
        "simplified"
    } else {
        output.detected_script.name()
    };
    let mut ai_signature = output.ai_signature;
    let mut translationese_signature = output.translationese_signature;
    let mut issues = output.issues;

    // 35.9 — Apply project glossary precedence (proper_noun suppression +
    // banned-term injection) before disambiguation, so the rest of the pipeline
    // sees the canonical issue list. Synthetic banned-term issues land with
    // `line: 0, col: 0` from `Issue::new`; reapply LineIndex so output
    // formatters and the 35.1 consistency report see correct coordinates.
    issues = zhtw_mcp::rules::glossary::apply_glossary_with_coordinates(
        &text,
        content_type,
        &cfg,
        issues,
        &params.glossary,
    );

    // Tier 2: local disambiguation.
    let disambig_cfg = zhtw_mcp::engine::disambig::DisambigConfig {
        profile,
        ..Default::default()
    };
    let _disambig_stats =
        zhtw_mcp::engine::disambig::disambiguate_batch(&mut issues, &text, &disambig_cfg);

    // Apply fixes if requested. Filter out TM-suppressed issues so the fixer
    // does not auto-correct terms the user deliberately rejected.
    let fix_result = if params.fix_mode != zhtw_mcp::fixer::FixMode::None {
        let fix_issues: Vec<_> = if let Some(ref tm) = tm_store {
            issues
                .iter()
                .filter(|i| !tm.should_suppress(&i.found))
                .cloned()
                .collect()
        } else {
            issues.clone()
        };

        // Write-side structure guard: the same exclusion ranges the MCP fix
        // path passes (src/mcp/tools.rs), built with the same options so the
        // two front ends cannot disagree about which bytes --fix may touch.
        // Scan-time exclusion is not enough, because a multi-part grammar match
        // can span an excluded region sitting between its parts, e.g. a fronted
        // object written as inline code sits between the parts.
        //
        // Rebuilt here rather than carried out of the scan because the scan
        // builds its ranges on the NFC-normalized text while the issues
        // reaching the fixer have been remapped back to original coordinates.
        // Nothing to fix means nothing to mask, and the Markdown build is a
        // second full parse of the document, so skip it on clean files.
        let excluded = if fix_issues.is_empty() {
            Vec::new()
        } else {
            zhtw_mcp::engine::scan::build_exclusions_for_content_type_with_config(
                &text,
                content_type,
                &cfg,
            )
        };
        Some(zhtw_mcp::fixer::apply_fixes_with_context(
            &text,
            &fix_issues,
            params.fix_mode,
            &excluded,
            Some(scanner.segmenter()),
        ))
    } else {
        None
    };

    // Writing the document and reporting on it are one step, kept in one
    // function. They used to be inline here, and the seam between "put the text
    // somewhere" and "tell the user what happened" is where the stdin
    // passthrough went wrong twice: once emitting nothing for an unchanged
    // document, once emitting it on top of a JSON report.
    let emitted = emit_fix_result(
        file_arg,
        &text,
        fix_result.as_ref(),
        input_was_sc,
        params,
        c,
    )?;
    let current_text = emitted.text;
    let wrote_changes = emitted.wrote_changes;

    // Count remaining issues after fix/S2T (rescan converted text). Single
    // rescan serves both issue reporting and AI signature refresh.
    let report_issues = if wrote_changes {
        let rescan_output =
            scanner.scan_for_content_type_with_config(current_text, content_type, cfg);
        // Refresh AI signature from the fixed text (avoids a second scan).
        let ai_active = cfg.ai_filler_detection
            || cfg.ai_semantic_safety
            || cfg.ai_density_detection
            || cfg.ai_structural_patterns;
        if ai_active {
            ai_signature = rescan_output.ai_signature;
        }
        if cfg.translationese_detection {
            translationese_signature = rescan_output.translationese_signature;
        }
        let mut rescan = rescan_output.issues;
        if let Some(ref fix) = fix_result {
            // Suppress convergent-chain noise from the fixer's own
            // replacements.
            zhtw_mcp::fixer::suppress_convergent_issues(&mut rescan, &fix.applied_fixes);
        }
        zhtw_mcp::rules::glossary::apply_glossary_with_coordinates(
            current_text,
            content_type,
            &cfg,
            rescan,
            &params.glossary,
        )
    } else {
        issues
    };

    // --verify: calibrate issues via Google Translate.
    #[cfg(feature = "translate")]
    let report_issues = if params.verify {
        let calibrate_text = if wrote_changes {
            current_text
        } else {
            text.as_str()
        };
        let mut issues_mut = report_issues;
        let result = zhtw_mcp::engine::translate::calibrate_issues(calibrate_text, &mut issues_mut);
        eprintln!(
            "{}  verify: {} matched, {} unmatched, {} no_english, api_ok={}{}",
            c.dim, result.matched, result.unmatched, result.no_english, result.api_ok, c.reset,
        );
        issues_mut
    } else {
        report_issues
    };

    // Apply TM suppressions. Shared with the MCP tool so the two front ends
    // cannot drift on which issue types the TM is allowed to touch.
    let mut report_issues = report_issues;
    let tm_suppressed = tm_store
        .as_ref()
        .map_or(0, |tm| tm.suppress_issues(&mut report_issues));

    // Project ignore_terms, applied after TM for the same reason and through
    // the same function the MCP tool calls: the term stays visible but drops to
    // Info, so it counts against neither gate.
    if !params.ignore_terms.is_empty() {
        let ignore_set: std::collections::HashSet<&str> =
            params.ignore_terms.iter().map(String::as_str).collect();
        zhtw_mcp::rules::ignore::apply_ignore_set(&mut report_issues, &ignore_set);
    }

    // --update-baseline: add all issues to the baseline.
    if params.update_baseline {
        for issue in &report_issues {
            state.baseline.insert(file_arg, issue);
        }
    }

    // --baseline: filter out baseline issues, count them separately.
    let new_issues: Vec<_> = if params.baseline_path.is_some() && !params.update_baseline {
        report_issues
            .iter()
            .filter(|i| {
                if state.baseline.contains(file_arg, i) {
                    state.baseline_count += 1;
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect()
    } else {
        report_issues.clone()
    };

    let error_count = new_issues
        .iter()
        .filter(|i| i.severity == zhtw_mcp::rules::ruleset::Severity::Error)
        .count();
    let warning_count = new_issues
        .iter()
        .filter(|i| i.severity == zhtw_mcp::rules::ruleset::Severity::Warning)
        .count();
    state.totals.errors += error_count;
    state.totals.warnings += warning_count;

    // Accumulate resolution tier stats from the final reported issues.
    for issue in &new_issues {
        use zhtw_mcp::rules::ruleset::ResolutionTier;
        match ResolutionTier::classify(issue) {
            ResolutionTier::Deterministic => state.totals.deterministic += 1,
            ResolutionTier::Heuristic => state.totals.heuristic += 1,
            ResolutionTier::LlmJudged => state.totals.llm_judged += 1,
            ResolutionTier::Unresolved => state.totals.unresolved += 1,
        }
    }

    // Use new_issues for reporting (baseline issues filtered out).
    let report_issues = new_issues;
    let report_text_char_count = if wrote_changes {
        fix_result
            .as_ref()
            .map_or(text_char_count, |f| f.text.chars().count())
    } else {
        text_char_count
    };

    let report = FileReport {
        file_arg,
        detected_script,
        issues: &report_issues,
        error_count,
        warning_count,
        tm_suppressed,
        fixes_applied: fix_result.as_ref().map(|f| f.applied),
        fixes_skipped: fix_result.as_ref().map(|f| f.skipped),
        fixes_declined: fix_result.as_ref().map(|f| f.declined),
        ai_signature: ai_signature.as_ref(),
        translationese_signature: translationese_signature.as_ref(),
        consistency_text: if wrote_changes {
            current_text
        } else {
            text.as_str()
        },
        text_char_count: report_text_char_count,
        multi,
    };

    match params.format {
        LintFormat::Json => {
            let output = render_json(&report, params);
            if multi {
                state.file_results.push(output);
            } else {
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        LintFormat::Human => render_human(&report, params, c),
        LintFormat::Compact => render_compact(&report, params.explain),
        LintFormat::Tabular => {
            render_tabular(&report, params.explain, &mut state.tabular_header_printed);
        }
        LintFormat::Sarif => {
            collect_sarif(&report, &mut state.sarif_rules, &mut state.sarif_results)
        }
    }
    Ok(())
}

// Convert subcommand: SC → TW pipeline

/// Built-in SC→TC conversion (character/phrase level via embedded OpenCC
/// dictionaries) then zhtw-mcp aggressive fix for context-aware zh-TW
/// phrase correction. No external OpenCC dependency required.
/// `verify` opts into the Google Translate anchor check, which sends the
/// sentences around each remaining issue off the machine.  Off by default:
/// conversion is otherwise entirely local, and a converter that phones home
/// unless told not to is the wrong default for anyone holding an unpublished
/// document.
fn run_convert(
    file_args: &[String],
    content_type_str: Option<&str>,
    overrides_path: PathBuf,
    #[cfg(feature = "translate")] verify: bool,
) -> Result<()> {
    use zhtw_mcp::engine::scan::{ContentType, Scanner};
    use zhtw_mcp::fixer::{apply_fixes_with_context, FixMode};
    use zhtw_mcp::rules::loader::load_embedded_ruleset;
    use zhtw_mcp::rules::store::OverrideStore;

    // Read input (files or stdin).
    let mut raw_input = String::new();
    for arg in file_args {
        if arg == "--" {
            std::io::stdin()
                .read_to_string(&mut raw_input)
                .context("failed to read stdin")?;
        } else {
            let content =
                std::fs::read_to_string(arg).with_context(|| format!("failed to read {arg}"))?;
            raw_input.push_str(&content);
        }
    }

    // Step 1: SC→TC character/phrase conversion (built-in, no OpenCC
    // dependency).
    let s2t = zhtw_mcp::engine::s2t::S2TConverter::new();
    let s2t_output = s2t.convert(&raw_input);

    // Step 2: Build scanner with overrides.
    let store = OverrideStore::open(&overrides_path)?;
    let ruleset = load_embedded_ruleset()?;
    let (spelling_rules, case_rules) = zhtw_mcp::rules::store::build_merged_rules(
        &ruleset.spelling_rules,
        &ruleset.case_rules,
        &store,
        &zhtw_mcp::rules::store::PackStore::new(zhtw_mcp::rules::store::default_packs_dir()),
        &[],
    );
    let scanner = Scanner::new(spelling_rules, case_rules);

    // Determine content type.
    let content_type = match content_type_str {
        Some("markdown" | "md") => ContentType::Markdown,
        Some("markdown-scan-code") => ContentType::MarkdownScanCode,
        Some("yaml" | "yml") => ContentType::Yaml,
        Some("plain") => ContentType::Plain,
        _ => {
            // Auto-detect from first file extension.
            let first_file = file_args.iter().find(|a| *a != "--");
            match first_file
                .and_then(|f| Path::new(f).extension())
                .and_then(|e| e.to_str())
            {
                Some("md") => ContentType::Markdown,
                Some("yml" | "yaml") => ContentType::Yaml,
                _ => ContentType::Plain,
            }
        }
    };

    // Step 3: Iterative fix loop — scan + fix until convergence or max rounds.
    let mut text = s2t_output;
    let max_rounds = 3;
    for round in 0..max_rounds {
        let excluded =
            zhtw_mcp::engine::scan::build_exclusions_for_content_type(&text, content_type);
        let scan_out = scanner.scan_with_prebuilt_excluded(
            &text,
            &excluded,
            zhtw_mcp::rules::ruleset::Profile::Base,
            content_type,
        );
        let issues = scan_out.issues;

        if issues.is_empty() {
            break;
        }

        let fix_result = apply_fixes_with_context(
            &text,
            &issues,
            FixMode::LexicalContextual,
            &excluded,
            Some(scanner.segmenter()),
        );

        if fix_result.applied == 0 {
            break;
        }

        eprintln!(
            "convert: round {} — {} issues, {} fixes applied",
            round + 1,
            issues.len(),
            fix_result.applied,
        );
        text = fix_result.text;
    }

    // Step 4: Optional verification via Google Translate. Requires --verify;
    // see the note on this function.
    #[cfg(feature = "translate")]
    if verify {
        let excluded =
            zhtw_mcp::engine::scan::build_exclusions_for_content_type(&text, content_type);
        let scan_out = scanner.scan_with_prebuilt_excluded(
            &text,
            &excluded,
            zhtw_mcp::rules::ruleset::Profile::Base,
            content_type,
        );
        let mut remaining = scan_out.issues;
        if !remaining.is_empty() {
            let cr = zhtw_mcp::engine::translate::calibrate_issues(&text, &mut remaining);
            eprintln!(
                "convert: verify — {} matched, {} unmatched, {} no_english, api_ok={}",
                cr.matched, cr.unmatched, cr.no_english, cr.api_ok,
            );
            let rejected_count = remaining
                .iter()
                .filter(|i| i.anchor_match == Some(false))
                .count();
            let no_signal_count = remaining
                .iter()
                .filter(|i| i.anchor_match.is_none() && i.english.is_some())
                .count();
            if rejected_count + no_signal_count > 0 {
                eprintln!(
                    "convert: {} residual issues ({} unconfirmed, {} no signal)",
                    rejected_count + no_signal_count,
                    rejected_count,
                    no_signal_count,
                );
            }
        }
    }

    // Output the corrected text.
    print!("{text}");

    Ok(())
}

// Setup subcommand

fn run_setup(host_str: &str) -> Result<()> {
    use zhtw_mcp::mcp::setup::{self, Host};

    let host = match Host::from_name(host_str) {
        Some(h) => h,
        None => {
            let hosts: Vec<&str> = setup::ALL_HOSTS.iter().map(|h| h.name()).collect();
            anyhow::bail!(
                "unknown host: '{host_str}'. Available: {}",
                hosts.join(", ")
            );
        }
    };

    let output = setup::generate_for_host(host);
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn run_translation_guide() -> Result<()> {
    let output = zhtw_mcp::mcp::setup::generate_translation_guide();
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

// Pack subcommand

fn run_tm_cmd(
    cmd: &str,
    arg: Option<&str>,
    tm_path: &std::path::Path,
    record_found: Option<&str>,
    record_suggested: Option<&str>,
    record_chose: Option<&str>,
    record_context: Option<&str>,
) -> Result<()> {
    use zhtw_mcp::rules::store::{iso_date_today, TmEntry, TranslationMemoryStore};

    match cmd {
        "list" => {
            let store = TranslationMemoryStore::open(tm_path)?;
            let entries = store.list();
            if entries.is_empty() {
                eprintln!("Translation memory is empty.");
            } else {
                let json = serde_json::to_string_pretty(entries)?;
                println!("{json}");
            }
            Ok(())
        }
        "export" => {
            let dest = arg.context("tm export requires a file path")?;
            let store = TranslationMemoryStore::open(tm_path)?;
            store.export(Path::new(dest))?;
            eprintln!("Exported TM ({} entries) to {dest}", store.list().len());
            Ok(())
        }
        "import" => {
            let src = arg.context("tm import requires a file path")?;
            let mut store = TranslationMemoryStore::open(tm_path)?;
            let (added, updated) = store.import(Path::new(src))?;
            eprintln!(
                "Imported {added} new, {updated} updated ({} total)",
                store.list().len()
            );
            Ok(())
        }
        "clear" => {
            let mut store = TranslationMemoryStore::open(tm_path)?;
            store.clear()?;
            eprintln!("Translation memory cleared.");
            Ok(())
        }
        "record" => {
            let found = record_found.context("tm record requires --found")?;
            let suggested = record_suggested.context("tm record requires --suggested")?;
            let chose = record_chose.context("tm record requires --chose")?;

            let mut store = TranslationMemoryStore::open(tm_path)?;
            store.record(TmEntry {
                found: found.to_string(),
                scanner_suggested: suggested.to_string(),
                user_chose: chose.to_string(),
                context: record_context.map(String::from),
                timestamp: iso_date_today(),
            })?;
            eprintln!("Recorded: '{found}' -> chose '{chose}'");
            Ok(())
        }
        _ => {
            anyhow::bail!(
                "unknown tm subcommand: '{cmd}' (expected list|export|import|clear|record)"
            );
        }
    }
}

fn run_pack_cmd(cmd: &str, arg: Option<&str>, packs_dir: &std::path::Path) -> Result<()> {
    use zhtw_mcp::rules::store::PackStore;

    let pack_store = PackStore::new(packs_dir.to_path_buf());

    match cmd {
        "list" => {
            let packs = pack_store.list();
            if packs.is_empty() {
                eprintln!("No packs installed in {}", packs_dir.display());
            } else {
                for pack in &packs {
                    let desc = pack
                        .metadata
                        .as_ref()
                        .and_then(|m| m.description.as_deref())
                        .unwrap_or("");
                    eprintln!(
                        "  {} ({} spelling, {} case){}",
                        pack.name,
                        pack.spelling_count,
                        pack.case_count,
                        if desc.is_empty() {
                            String::new()
                        } else {
                            format!(" — {desc}")
                        },
                    );
                }
            }
            Ok(())
        }
        "import" => {
            let source = arg.context("pack import requires a file path")?;
            let source_path = std::path::Path::new(source);
            let name = source_path
                .file_stem()
                .context("cannot determine pack name from file path")?
                .to_string_lossy();
            pack_store.install(&name, source_path)?;
            eprintln!("Installed pack '{name}' to {}", packs_dir.display());
            Ok(())
        }
        "export" => {
            let name = arg.context("pack export requires a pack name")?;
            let dest = format!("{name}.json");
            pack_store.export(name, std::path::Path::new(&dest))?;
            eprintln!("Exported pack '{name}' to {dest}");
            Ok(())
        }
        "validate" => {
            let file = arg.context("pack validate requires a file path")?;
            let warnings = PackStore::validate(std::path::Path::new(file))?;
            if warnings.is_empty() {
                eprintln!("Pack is valid.");
            } else {
                for w in &warnings {
                    eprintln!("  warning: {w}");
                }
                eprintln!("{} warning(s).", warnings.len());
            }
            Ok(())
        }
        _ => {
            anyhow::bail!(
                "unknown pack subcommand: '{cmd}' (expected import|export|validate|list)"
            );
        }
    }
}

// Helpers

// Diff-from: resolve changed files via git

/// Resolve files changed since a given git ref.
fn resolve_diff_files(git_ref: &str) -> Result<Vec<String>> {
    // Reject refs starting with - to prevent git flag injection. Command::new
    // does not invoke a shell, but a ref like --output=x would still be
    // interpreted as a git flag by the subprocess.
    anyhow::ensure!(
        !git_ref.starts_with('-'),
        "--diff-from ref must not start with '-'"
    );
    anyhow::ensure!(
        git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-~^@{}".contains(c)),
        "--diff-from ref contains invalid characters"
    );

    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", &format!("{git_ref}...HEAD")])
        .output()
        .context("run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| {
            // Only include supported extensions.
            let lower = l.to_ascii_lowercase();
            lower
                .rsplit_once('.')
                .is_some_and(|(_, ext)| SUPPORTED_EXTENSIONS.contains(&ext))
        })
        .map(String::from)
        .collect();

    Ok(files)
}

// Directory walking for multi-file linting

/// Supported file extensions for recursive directory discovery.
const SUPPORTED_EXTENSIONS: &[&str] = &["md", "markdown", "yml", "yaml", "txt"];

/// Resolve a list of file/directory arguments into a deduplicated, sorted list
/// of file paths.  Directories are expanded recursively; hidden entries and
/// symlinks are skipped; --exclude patterns are applied.
fn resolve_file_args(args: &[String], exclude: &[String]) -> Result<Vec<String>> {
    let mut files = BTreeSet::new();

    for arg in args {
        if arg == "--" {
            // stdin sentinel — pass through as-is.
            files.insert("--".to_string());
            continue;
        }

        let path = Path::new(arg);
        if !path.exists() {
            anyhow::bail!("path does not exist: {arg}");
        }

        if path.is_dir() {
            walk_directory(path, &mut files, exclude)?;
        } else if path.is_file() {
            let canonical = normalize_path(path);
            if !is_excluded(&canonical, exclude) {
                files.insert(canonical);
            }
        }
        // Skip symlinks and other non-file/non-dir entries.
    }

    if files.is_empty() {
        anyhow::bail!("no supported files found in the given paths");
    }

    Ok(files.into_iter().collect())
}

/// Recursively walk a directory, collecting supported files.
fn walk_directory(dir: &Path, files: &mut BTreeSet<String>, exclude: &[String]) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("read directory: {}", dir.display()))?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                eprintln!("warning: {}: {err}", dir.display());
                None
            }
        })
        .collect();

    // Deterministic: sort entries lexicographically by file name.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let ft = entry
            .file_type()
            .with_context(|| format!("file type: {}", entry.path().display()))?;

        // Skip symlinks.
        if ft.is_symlink() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden files/directories.
        if name_str.starts_with('.') {
            continue;
        }

        let path = entry.path();

        if ft.is_dir() {
            walk_directory(&path, files, exclude)?;
        } else if ft.is_file() {
            // Check extension.
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = ext.to_ascii_lowercase();
                if SUPPORTED_EXTENSIONS.contains(&ext_lower.as_str()) {
                    let canonical = normalize_path(&path);
                    if !is_excluded(&canonical, exclude) {
                        files.insert(canonical);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Normalize a path to a string for consistent deduplication.
fn normalize_path(path: &Path) -> String {
    match path.canonicalize() {
        Ok(abs) => abs.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Check if a file path matches any --exclude pattern.
///
/// Supported patterns:
/// - *.ext — match files with the given extension
/// - dir/** — match anything under the given directory component
/// - Literal path-component match as a fallback
fn is_excluded(path: &str, patterns: &[String]) -> bool {
    for pat in patterns {
        if pat.starts_with("*.") {
            // Extension match: *.tmp, *.bak
            let ext = &pat[1..]; // ".tmp"
            if path.ends_with(ext) {
                return true;
            }
        } else if pat.ends_with("/**") {
            // Directory component match: vendor/** matches
            // /path/to/vendor/file.md but not /path/to/some_vendor/file.md.
            let prefix = &pat[..pat.len() - 3];
            let sep_prefix = format!("/{prefix}/");
            if path.contains(&sep_prefix) || path.ends_with(&format!("/{prefix}")) {
                return true;
            }
        } else {
            // Path-component match: check if any path component equals the
            // pattern.
            let sep_pat = format!("/{pat}/");
            if path.contains(&sep_pat)
                || path.ends_with(&format!("/{pat}"))
                || path.starts_with(&format!("{pat}/"))
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a command line written without the program name.
    fn parse(argv: &[&str]) -> Result<Cli> {
        let mut args = vec!["zhtw-mcp".to_string()];
        args.extend(argv.iter().map(|s| s.to_string()));
        parse_args(&args)
    }

    fn lint_of(argv: &[&str]) -> LintArgs {
        match parse(argv).expect("parse should succeed").command {
            Command::Lint(l) => *l,
            _ => panic!("expected a lint command from {argv:?}"),
        }
    }

    fn convert_of(argv: &[&str]) -> ConvertArgs {
        match parse(argv).expect("parse should succeed").command {
            Command::Convert(c) => c,
            _ => panic!("expected a convert command from {argv:?}"),
        }
    }

    fn tm_of(argv: &[&str]) -> TmArgs {
        match parse(argv).expect("parse should succeed").command {
            Command::Tm(t) => t,
            _ => panic!("expected a tm command from {argv:?}"),
        }
    }

    fn err_of(argv: &[&str]) -> String {
        parse(argv)
            .err()
            .unwrap_or_else(|| panic!("expected {argv:?} to fail"))
            .to_string()
    }

    #[test]
    fn no_args_runs_the_server() {
        let cli = parse(&[]).unwrap();
        assert!(matches!(cli.command, Command::Server));
        assert!(cli.overrides_path.is_none());
        assert!(cli.active_packs.is_empty());
    }

    #[test]
    fn global_flags_precede_the_subcommand() {
        let cli = parse(&[
            "--pack",
            "medical",
            "--pack",
            "legal",
            "--overrides",
            "/tmp/o.json",
            "--suppressions",
            "/tmp/s.json",
            "--packs-dir",
            "/tmp/packs",
            "--config",
            "/tmp/c.toml",
            "lint",
            "a.md",
        ])
        .unwrap();
        assert_eq!(cli.active_packs, ["medical", "legal"]);
        assert_eq!(cli.overrides_path.unwrap().to_str(), Some("/tmp/o.json"));
        assert_eq!(cli.suppressions_path.unwrap().to_str(), Some("/tmp/s.json"));
        assert_eq!(cli.packs_dir.unwrap().to_str(), Some("/tmp/packs"));
        assert_eq!(cli.config_path.unwrap().to_str(), Some("/tmp/c.toml"));
    }

    #[test]
    fn global_flags_require_their_value() {
        for flag in [
            "--overrides",
            "--pack",
            "--packs-dir",
            "--suppressions",
            "--config",
        ] {
            assert!(parse(&[flag]).is_err(), "{flag} without a value must fail");
        }
    }

    #[test]
    fn lint_collects_files_and_defaults() {
        let lint = lint_of(&["lint", "a.md", "b.md"]);
        assert_eq!(lint.files, ["a.md", "b.md"]);
        assert!(matches!(lint.format, LintFormat::Human));
        assert_eq!(lint.max_errors, None);
        assert!(lint.fix_mode.is_none());
        assert!(!lint.detect_ai);
    }

    #[test]
    fn lint_without_files_fails() {
        assert!(err_of(&["lint"]).contains("at least one file"));
        assert!(err_of(&["lint", "--relaxed"]).contains("at least one file"));
    }

    #[test]
    fn lint_formats() {
        for (name, want) in [
            ("json", LintFormat::Json),
            ("human", LintFormat::Human),
            ("sarif", LintFormat::Sarif),
            ("compact", LintFormat::Compact),
            ("tabular", LintFormat::Tabular),
        ] {
            let lint = lint_of(&["lint", "a.md", "--format", name]);
            assert_eq!(
                std::mem::discriminant(&lint.format),
                std::mem::discriminant(&want),
                "--format {name}"
            );
        }
        assert!(err_of(&["lint", "a.md", "--format", "xml"]).contains("unknown format"));
        assert!(err_of(&["lint", "a.md", "--format"]).contains("requires a value"));
    }

    #[test]
    fn lint_numeric_flags() {
        let lint = lint_of(&["lint", "a.md", "--max-errors", "3", "--max-warnings", "7"]);
        assert_eq!(lint.max_errors, Some(3));
        assert_eq!(lint.max_warnings, Some(7));
        assert!(err_of(&["lint", "a.md", "--max-errors", "x"]).contains("--max-errors"));
        assert!(err_of(&["lint", "a.md", "--max-warnings", "-1"]).contains("--max-warnings"));
    }

    #[test]
    fn lint_fix_modes() {
        use zhtw_mcp::fixer::FixMode;
        for (arg, want) in [
            ("--fix", FixMode::LexicalSafe),
            ("--fix=lexical_safe", FixMode::LexicalSafe),
            ("--fix=orthographic", FixMode::Orthographic),
            ("--fix=lexical_contextual", FixMode::LexicalContextual),
        ] {
            let lint = lint_of(&["lint", "a.md", arg]);
            assert_eq!(lint.fix_mode, Some(want), "{arg}");
        }
        assert!(err_of(&["lint", "a.md", "--fix=wild"]).contains("unknown fix mode"));
    }

    #[test]
    fn lint_content_type_is_validated() {
        for ct in ["plain", "markdown", "markdown-scan-code", "yaml"] {
            let lint = lint_of(&["lint", "a.md", "--content-type", ct]);
            assert_eq!(lint.content_type.as_deref(), Some(ct));
        }
        assert!(err_of(&["lint", "a.md", "--content-type", "rst"]).contains("unknown content-type"));
    }

    #[test]
    fn detect_ai_level_is_optional() {
        // No level: the next token stays a file path.
        let lint = lint_of(&["lint", "--detect-ai", "a.md"]);
        assert!(lint.detect_ai);
        assert_eq!(lint.files, ["a.md"]);
        assert_eq!(lint.ai_threshold_multiplier, 1.0);

        for (level, mult) in [("low", 0.5), ("medium", 1.0), ("high", 1.5)] {
            let lint = lint_of(&["lint", "--detect-ai", level, "a.md"]);
            assert_eq!(lint.ai_threshold_multiplier, mult, "--detect-ai {level}");
            assert_eq!(lint.files, ["a.md"], "--detect-ai {level} ate the file");
        }
    }

    #[test]
    fn a_later_flag_without_a_level_keeps_the_earlier_one() {
        // --detect-style carries no level here, so it must not reset the low
        // threshold --detect-ai already requested. Order must not matter.
        let a = lint_of(&[
            "lint",
            "a.md",
            "--detect-ai",
            "low",
            "--detect-style",
            "--format",
            "json",
        ]);
        let b = lint_of(&[
            "lint",
            "a.md",
            "--detect-style",
            "--detect-ai",
            "low",
            "--format",
            "json",
        ]);
        assert_eq!(a.ai_threshold_multiplier, 0.5);
        assert_eq!(b.ai_threshold_multiplier, 0.5);

        // An explicit level still wins, whichever flag carries it.
        let c = lint_of(&[
            "lint",
            "a.md",
            "--detect-ai",
            "low",
            "--detect-style",
            "high",
            "--format",
            "json",
        ]);
        assert_eq!(c.ai_threshold_multiplier, 1.5);
    }

    #[test]
    fn detect_style_implies_both_axes_and_needs_json() {
        let lint = lint_of(&["lint", "a.md", "--detect-style", "--format", "json"]);
        assert!(lint.detect_style && lint.detect_ai && lint.detect_translationese);
        assert!(err_of(&["lint", "a.md", "--detect-style"]).contains("--format json"));
    }

    #[test]
    fn translationese_domain_is_validated() {
        let lint = lint_of(&["lint", "a.md", "--translationese-domain", "technical"]);
        assert!(matches!(
            lint.translationese_domain,
            zhtw_mcp::engine::translationese_score::TranslationeseDomain::Technical
        ));
        assert!(
            err_of(&["lint", "a.md", "--translationese-domain", "poetic"])
                .contains("unknown --translationese-domain")
        );
        assert!(err_of(&["lint", "--translationese-domain"]).contains("requires a value"));
    }

    #[test]
    fn lint_boolean_flags() {
        let lint = lint_of(&[
            "lint",
            "a.md",
            "--relaxed",
            "--exempt-blockquotes",
            "--consistency",
            "--dry-run",
            "--explain",
            "--update-baseline",
            "--telemetry",
            "--detect-translationese",
        ]);
        assert!(lint.relaxed);
        assert!(lint.exempt_blockquotes);
        assert!(lint.consistency);
        assert!(lint.dry_run);
        assert!(lint.explain);
        assert!(lint.update_baseline);
        assert!(lint.telemetry);
        assert!(lint.detect_translationese);
    }

    #[test]
    fn lint_path_flags() {
        let lint = lint_of(&[
            "lint",
            "a.md",
            "--baseline",
            "base.json",
            "--diff-from",
            "origin/main",
            "--exclude",
            "vendor/**",
            "--exclude",
            "*.tmp",
            "--profile",
            "strict",
        ]);
        assert_eq!(lint.baseline_path.unwrap().to_str(), Some("base.json"));
        assert_eq!(lint.diff_from.as_deref(), Some("origin/main"));
        assert_eq!(lint.exclude_patterns, ["vendor/**", "*.tmp"]);
        assert_eq!(lint.profile.as_deref(), Some("strict"));
    }

    #[test]
    fn lint_treats_unknown_flags_as_file_paths() {
        // Documented behavior: global flags belong before the subcommand, so
        // anything unrecognized after `lint` is a path, not an error.
        let lint = lint_of(&["lint", "--pack", "medical"]);
        assert_eq!(lint.files, ["--pack", "medical"]);
    }

    #[test]
    fn lint_accepts_stdin_and_log_flags() {
        let lint = lint_of(&["lint", "--", "--verbose", "--debug"]);
        assert_eq!(lint.files, ["--"]);
    }

    #[cfg(feature = "translate")]
    #[test]
    fn verify_flag_is_recognized_when_the_feature_is_on() {
        assert!(lint_of(&["lint", "a.md", "--verify"]).verify);
        assert!(convert_of(&["convert", "--verify"]).verify);
    }

    #[cfg(not(feature = "translate"))]
    #[test]
    fn verify_flag_explains_the_missing_feature() {
        assert!(err_of(&["lint", "a.md", "--verify"]).contains("translate"));
        assert!(err_of(&["convert", "--verify"]).contains("translate"));
    }

    #[test]
    fn convert_defaults_to_stdin() {
        assert_eq!(convert_of(&["convert"]).files, ["--"]);
    }

    #[test]
    fn convert_validates_content_type_like_lint() {
        assert_eq!(
            convert_of(&["convert", "a.md", "--content-type", "yaml"])
                .content_type
                .as_deref(),
            Some("yaml")
        );

        // The two abbreviations convert accepted before the validator was
        // shared still work, normalized to the long form.
        for (given, want) in [("md", "markdown"), ("yml", "yaml")] {
            assert_eq!(
                convert_of(&["convert", "a.md", "--content-type", given])
                    .content_type
                    .as_deref(),
                Some(want)
            );
            assert_eq!(
                lint_of(&["lint", "a.md", "--content-type", given])
                    .content_type
                    .as_deref(),
                Some(want)
            );
        }
        // Used to be accepted and silently fall through to auto-detection.
        assert!(err_of(&["convert", "a.md", "--content-type", "markdwon"])
            .contains("unknown content-type"));
        assert!(err_of(&["convert", "a.md", "--content-type"]).contains("requires a value"));
    }

    #[test]
    fn convert_collects_files_and_rejects_unknown_flags() {
        let convert = convert_of(&["convert", "a.md", "--content-type", "markdown"]);
        assert_eq!(convert.files, ["a.md"]);
        assert_eq!(convert.content_type.as_deref(), Some("markdown"));
        assert!(err_of(&["convert", "--nope"]).contains("unknown convert flag"));
    }

    #[test]
    fn tm_record_collects_key_values() {
        let tm = tm_of(&[
            "tm",
            "record",
            "--found",
            "軟件",
            "--suggested",
            "軟體",
            "--chose",
            "軟體",
            "--context",
            "句子",
        ]);
        assert_eq!(tm.cmd, "record");
        assert_eq!(tm.found.as_deref(), Some("軟件"));
        assert_eq!(tm.suggested.as_deref(), Some("軟體"));
        assert_eq!(tm.chose.as_deref(), Some("軟體"));
        assert_eq!(tm.context.as_deref(), Some("句子"));
        assert!(err_of(&["tm", "record", "--bogus", "x"]).contains("unknown tm record flag"));
        assert!(err_of(&["tm", "record", "--found"]).contains("--found requires a value"));
    }

    #[test]
    fn tm_argument_consumption_is_per_subcommand() {
        for sub in ["export", "import"] {
            assert_eq!(tm_of(&["tm", sub, "f.json"]).arg.as_deref(), Some("f.json"));
            assert!(err_of(&["tm", sub]).contains("requires a file path"));
        }
        for sub in ["list", "clear"] {
            assert!(tm_of(&["tm", sub]).arg.is_none());
        }
        assert!(err_of(&["tm"]).contains("tm requires a subcommand"));
    }

    #[test]
    fn pack_argument_consumption_is_per_subcommand() {
        for sub in ["import", "export", "validate"] {
            match parse(&["pack", sub, "x"]).unwrap().command {
                Command::Pack { cmd, arg } => {
                    assert_eq!(cmd, sub);
                    assert_eq!(arg.as_deref(), Some("x"));
                }
                _ => panic!("expected pack"),
            }
            assert!(err_of(&["pack", sub]).contains("requires an argument"));
        }
        match parse(&["pack", "list"]).unwrap().command {
            Command::Pack { cmd, arg } => {
                assert_eq!(cmd, "list");
                assert!(arg.is_none());
            }
            _ => panic!("expected pack"),
        }
        assert!(err_of(&["pack"]).contains("pack requires a subcommand"));
    }

    #[test]
    fn cache_clear_takes_no_extra_arguments() {
        assert!(matches!(
            parse(&["cache", "clear"]).unwrap().command,
            Command::CacheClear
        ));
        assert!(err_of(&["cache", "clear", "all"]).contains("does not accept additional"));
        assert!(err_of(&["cache", "purge"]).contains("unknown cache subcommand"));
        assert!(err_of(&["cache"]).contains("cache requires a subcommand"));
    }

    #[test]
    fn setup_requires_a_host() {
        match parse(&["setup", "claude"]).unwrap().command {
            Command::Setup(h) => assert_eq!(h, "claude"),
            _ => panic!("expected setup"),
        }
        assert!(err_of(&["setup"]).contains("requires a host name"));
    }

    #[test]
    fn a_second_subcommand_is_rejected() {
        // The flat-variable version silently resolved this by dispatch order.
        assert!(err_of(&["setup", "claude", "pack", "list"]).contains("only one subcommand"));
        assert!(err_of(&["pack", "list", "cache", "clear"]).contains("only one subcommand"));
    }

    #[test]
    fn unknown_top_level_argument_is_rejected() {
        assert!(err_of(&["--nope"]).contains("unknown argument"));
        assert!(err_of(&["frobnicate"]).contains("unknown argument"));
        // --content-type is lint-only, so it is unknown at the top level.
        assert!(err_of(&["--content-type", "markdown"]).contains("unknown argument"));
    }

    #[test]
    fn log_level_flags_are_accepted_anywhere() {
        assert!(matches!(
            parse(&["--verbose", "--debug"]).unwrap().command,
            Command::Server
        ));
    }
}
