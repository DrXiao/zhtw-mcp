# CLI usage

## Linting files

```bash
# Single file
zhtw-mcp lint README.md

# Multiple files and directories (recursive)
zhtw-mcp lint docs/ src/locales/ README.md

# Stdin
zhtw-mcp lint -- < input.txt

# With options
zhtw-mcp lint file.md --format json --profile strict --max-errors 5
zhtw-mcp lint file.md --telemetry           # print stderr summary counters
zhtw-mcp lint file.md --format tabular              # aligned columns
zhtw-mcp lint docs/ --exclude "vendor/**"
zhtw-mcp lint -- --content-type markdown < input.md
zhtw-mcp lint -- --content-type markdown-scan-code < input.md  # also lint inside code blocks
```

## Auto-fix

```bash
zhtw-mcp lint file.md --fix                        # lexical_safe (default)
zhtw-mcp lint file.md --fix=orthographic           # punctuation/spacing/case/variant/grammar only
zhtw-mcp lint file.md --fix=lexical_contextual     # context-clue-gated rules too
zhtw-mcp lint file.md --fix --dry-run       # preview without writing
```

## Explaining flagged terms

```bash
zhtw-mcp lint file.md --explain
```

Each issue includes a cultural/linguistic annotation and its English anchor term.

## Inline suppression

A pragma is `zhtw:` plus a keyword, placed behind whatever comment opener the file already uses: `<!--` (Markdown, HTML), `//` (C-family), or `#` (YAML, TOML, Python, shell, locale files). Every keyword works behind every opener.

Two limits on `#`: it is ignored under `--content-type markdown`, where `#` starts a heading rather than a comment, and it must begin its own token, so `key: value# zhtw:ignore` is data rather than a pragma.

| Keyword | Effect |
|---|---|
| `zhtw:ignore`, `zhtw:ignore-line` | suppress the line the pragma sits on |
| `zhtw:ignore-next-line`, `zhtw:ignore-next` | suppress the following line |
| `zhtw:ignore-block` ... `zhtw:end-ignore` | suppress everything between the two markers |

Every keyword has a `disable` spelling (`zhtw:disable-next-line`, `zhtw:disable-block`), and `zhtw:enable` is accepted as a block end, for users coming from linters that pair disable with enable. A bare `zhtw:disable` suppresses one line rather than opening a block; use `zhtw:disable-block` to fence off a region.

```yaml
title: 用戶手冊  # zhtw:ignore

# zhtw:disable-block
mainland_samples:
  - 用戶
  - 數據庫
# zhtw:enable
```

```markdown
<!-- zhtw:ignore-next-line -->
這行不會被檢查。
```

An unclosed block runs to the end of the file. An unrecognized keyword suppresses nothing, so a typo fails loudly (the issue still fires) rather than silently muting a region.

## Scan caching

In lint-only mode (no `--fix`), the CLI automatically caches scan results keyed by file content hash (BLAKE3) and scan parameters. Unchanged files are skipped on subsequent runs. The cache lives at the platform default cache directory (`~/.cache/zhtw-mcp/` on Linux, `~/Library/Caches/zhtw-mcp/` on macOS) with 24-hour TTL and a 2000-entry cap. Caching is disabled when `--fix`, `--verify`, or stdin mode is active.

## Telemetry

Use `--telemetry` with `lint` to print a compact stderr summary after the run:

```bash
zhtw-mcp lint docs/ --telemetry
```

This reports processed file count plus total error/warning counts. It does not change stdout formatting or exit-code behavior.

## Judgment cache

LLM-backed disambiguation decisions are also persisted in a separate judgment cache. To clear it:

```bash
zhtw-mcp cache clear
```

## Output formats

| Format | Flag | Description |
|--------|------|-------------|
| `human` | _(default)_ | Colored, multi-line output for terminals |
| `json` | `--format json` | Machine-readable JSON array |
| `compact` | `--format compact` | One line per issue |
| `tabular` | `--format tabular` | Aligned columns for quick scanning |
| `sarif` | `--format sarif` | SARIF v2.1.0 for GitHub Code Scanning |

## CI/CD integration

```bash
# SARIF output for GitHub Code Scanning
zhtw-mcp lint docs/ --format sarif > results.sarif

# Baseline mode: suppress known issues, fail only on new ones
zhtw-mcp lint docs/ --baseline baseline.json

# Lint only files changed since a branch
zhtw-mcp lint --diff-from main
```

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Linting completed and the text stayed within `--max-errors` and `--max-warnings`. |
| 1 | Linting completed and the text failed a gate. The findings are on stdout. |
| 2 | The run failed: bad arguments, unreadable config, or a file that could not be processed. Any findings printed are incomplete. |

A file that cannot be read (not UTF-8, over the 16 MiB limit, permission denied)
is reported on stderr and skipped; the rest of the batch is still linted and
still reported. Because the gate was then computed over an incomplete set, the
run exits 2 rather than 0 or 1, so a green build cannot come from a file nobody
managed to read.

## Project config file

Create `.zhtw-mcp.toml` at your project root for team-wide settings:

```toml
profile = "strict"
max_errors = 0
max_warnings = 10
exclude = ["vendor/**", "*.bak"]
packs = ["medical"]
```

Discovered by walking from cwd upward to the `.git` root. CLI flags override config values. Supported fields: `profile`, `relaxed`, `content_type`, `max_errors`, `max_warnings`, `ignore_terms`, `exclude`, `overrides`, `suppressions`, `packs`, `translation_memory`, plus the `[markdown]` and `[glossary]` sections.

`ignore_terms` keeps matching issues in the output but drops them to `info`, so they count against neither `max_errors` nor `max_warnings`. `overrides`, `suppressions` and `translation_memory` name the store files; all three are also read in server mode, so an MCP client needs no flags and the server answers from the same stores `lint` reads.

## Converting Simplified Chinese to Traditional

The `convert` subcommand converts Simplified Chinese (zh-CN) text to Traditional Chinese (zh-TW) and then applies the full lint/fix pipeline to normalize vocabulary:

```bash
# Convert a file (writes corrected output to stdout)
zhtw-mcp convert file.md

# Convert from stdin
zhtw-mcp convert -- < input.txt

# Specify content type explicitly
zhtw-mcp convert file.md --content-type markdown
```

This is a two-stage pipeline: first a built-in character/phrase converter (SC→TC), then iterative vocabulary normalization via the standard scanner.

### `--verify` sends text off the machine

When the `translate` feature is enabled (it is, by default), `lint` and `convert` both accept `--verify` to confirm ambiguous substitutions against English anchor terms. This is the only part of either subcommand that touches the network: it sends the sentence around each unresolved issue, up to 4 KB per run, to `translate.googleapis.com`. Nothing else in the tool leaves the machine.

It is off unless you pass the flag. Build with `--no-default-features` (plus the features you want) to remove the capability entirely; `--verify` then fails with an explanatory error rather than silently doing nothing.

## Editor integration setup

Generate configuration snippets for MCP-capable editors:

```bash
zhtw-mcp setup claude-code
zhtw-mcp setup codex
```

Prints JSON configuration for the specified host. Available hosts depend on the build.

## Pre-commit hook

Add to your `.pre-commit-config.yaml`:

```yaml
- repo: https://github.com/<org>/zhtw-mcp
  hooks:
    - id: zhtw-mcp
```

The hook runs `zhtw-mcp lint` on staged Markdown, YAML, and text files.

## Rule packs

Domain-specific rule overlays stored as JSON files in the `packs/` subdirectory. Same schema as `overrides.json`. Layered on top of the base ruleset in `--pack` flag order.

```bash
zhtw-mcp pack import medical.json   # install a pack
zhtw-mcp pack export medical         # export a pack to medical.json
zhtw-mcp pack validate medical.json  # validate schema and check for issues
zhtw-mcp pack list                   # list installed packs
zhtw-mcp --pack medical lint file.md # activate pack for a lint run
zhtw-mcp --pack medical --pack legal # multiple packs
```
