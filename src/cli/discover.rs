// File discovery for the lint and convert subcommands: turning what the user
// typed on the command line into the list of files actually scanned.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

// Diff-from: resolve changed files via git

/// Resolve files changed since a given git ref.
///
/// Returns absolute paths that still exist. A diff names the old side of a
/// rename and every deleted file too, and neither can be linted.
pub(crate) fn resolve_diff_files(git_ref: &str) -> Result<Vec<String>> {
    // Reject refs starting with - to prevent git flag injection. Command::new
    // does not invoke a shell, but a ref like --output=x would still be
    // interpreted as a git flag by the subprocess.
    anyhow::ensure!(
        !git_ref.starts_with('-'),
        "--diff-from ref must not start with '-'"
    );

    // Everything git-check-ref-format permits, minus the shell-ish characters
    // no ref needs. `+` belongs here: a branch may legitimately be named
    // `feature/foo+bar`, and rejecting it turned a valid ref into an error.
    anyhow::ensure!(
        git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-~^@{}+".contains(c)),
        "--diff-from ref contains invalid characters"
    );

    // Paths come back relative to the repository root, not the directory the
    // command ran in, so resolve them against the root rather than the cwd.
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("run git rev-parse --show-toplevel")?;
    anyhow::ensure!(
        top.status.success(),
        "git rev-parse --show-toplevel failed: {}",
        String::from_utf8_lossy(&top.stderr)
    );
    let root = Path::new(String::from_utf8_lossy(&top.stdout).trim()).to_path_buf();

    // -z gives NUL-terminated, unquoted names. Without it git applies
    // core.quotepath and wraps anything non-ASCII or containing a newline in
    // C-style quotes, which this then tried to open literally.
    let output = std::process::Command::new("git")
        .args(["diff", "-z", "--name-only", &format!("{git_ref}...HEAD")])
        .output()
        .context("run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter(|l| has_supported_extension(l))
        .map(|l| root.join(l))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    Ok(files)
}

// Directory walking for multi-file linting

/// Supported file extensions for recursive directory discovery.
const SUPPORTED_EXTENSIONS: &[&str] = &["md", "markdown", "yml", "yaml", "txt"];

/// One extension test for both discovery paths: the git diff list and the
/// directory walk had separate copies that could accept different files.
fn has_supported_extension(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Resolve a list of file/directory arguments into a deduplicated, sorted list
/// of file paths.  Directories are expanded recursively; hidden entries and
/// symlinks are skipped; --exclude patterns are applied.
pub(crate) fn resolve_file_args(args: &[String], exclude: &[String]) -> Result<Vec<String>> {
    let mut files = BTreeSet::new();

    for arg in args {
        if arg == "--" {
            // stdin sentinel — pass through as-is.
            files.insert("--".to_string());
            continue;
        }

        let path = Path::new(arg);

        // symlink_metadata rather than exists()/is_file(): both follow the
        // link, so a symlink passed directly was scanned even though the
        // directory walk skips them and this function promises the same.
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            anyhow::bail!("path does not exist: {arg}");
        };
        if meta.file_type().is_symlink() {
            continue;
        }

        if meta.is_dir() {
            walk_directory(path, &mut files, exclude)?;
        } else if meta.is_file() {
            let canonical = normalize_path(path);
            if !path_excluded(&canonical, exclude) {
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
            // Tested before descending, not just on the files inside: an
            // excluded directory was still walked in full, so a large or
            // unreadable vendor tree cost time or failed the run outright
            // despite being excluded.
            if path_excluded(&normalize_path(&path), exclude) {
                continue;
            }
            walk_directory(&path, files, exclude)?;
        } else if ft.is_file() {
            // Check extension.
            if has_supported_extension(&path.to_string_lossy()) {
                let canonical = normalize_path(&path);
                if !path_excluded(&canonical, exclude) {
                    files.insert(canonical);
                }
            }
        }
    }

    Ok(())
}

/// Normalize a path to a string for consistent deduplication.
///
/// `dunce::canonicalize` rather than `Path::canonicalize`: on Windows the
/// latter returns the `\\?\`-prefixed verbatim form, which does not
/// string-match the plain paths this crate compares it against elsewhere
/// (`std::env::current_dir()` in `cli::render`, `--exclude` patterns in
/// `path_excluded` below). `dunce` canonicalizes the same way but drops the
/// prefix when the plain form is sufficient; on non-Windows it is a
/// pass-through to `std::fs::canonicalize`.
fn normalize_path(path: &Path) -> String {
    match dunce::canonicalize(path) {
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
fn path_excluded(path: &str, patterns: &[String]) -> bool {
    // Patterns are always written with `/` (`vendor/**`, doc comment above),
    // but `path` carries the OS separator; on Windows that's `\`, so every
    // `/`-delimited comparison below missed every match. Normalize once
    // rather than teach each branch two separators.
    let path = path.replace('\\', "/");
    let path = path.as_str();
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
