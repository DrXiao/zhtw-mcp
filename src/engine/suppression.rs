// Inline suppression mechanism for zhtw-mcp linting.
//
// A marker is the token "zhtw:" plus a keyword, sitting behind whatever comment
// opener the file already uses. Three openers are recognized: "<!--" (Markdown,
// HTML), "//" (C-family), and "#" (YAML, TOML, Python, shell, locale files);
// the "#" opener is off for Markdown content, where "#" starts a heading. Every
// keyword works behind every opener, so the same pragma vocabulary applies to
// any content type:
//
//   <!-- zhtw:ignore-next-line -->    suppress the following line
//   key: value  # zhtw:ignore         suppress this line
// # zhtw:ignore-block               suppress until the closing marker
// # zhtw:end-ignore
//
// The disable spellings are aliases for the ignore spellings, for users who
// prefer the disable/enable terminology common in other linters (e.g.
// eslint-disable-next-line); "zhtw:enable" closes a block for the same reason.
// Note that a bare "zhtw:disable" suppresses one line rather than opening a
// block, matching what this crate has always done for "// zhtw:disable"; use
// "zhtw:disable-block" to fence off a region.
//
// Markers are recognized lexically, with no knowledge of string literals. A
// quoted value that contains a pragma, YAML `key: "see # zhtw:ignore"`,
// suppresses its own line. Tracking that would mean a string lexer per content
// type inside a scanner whose whole job is finding one token, and the failure
// needs a document that quotes a real pragma keyword, so the limitation is
// documented rather than fixed.
//
// Suppressed ranges are merged into excluded ranges before scanning.

use super::excluded::ByteRange;

/// What a marker does to the text around it.
#[derive(Clone, Copy)]
enum Marker {
    /// Suppress the line following the marker's own line.
    NextLine,
    /// Open a suppressed block.
    BlockStart,
    /// Close a suppressed block.
    BlockEnd,
    /// Suppress the marker's own line, from the start of that line.
    Line,
}

/// Returns true when `head`, the text before a "zhtw:" token with trailing
/// spaces removed, ends in a comment opener.
///
/// Both slash and hash openers must start their own token.  For `//` that
/// keeps a URL out: in `https://zhtw:ignore` the scheme separator is not a
/// comment, and neither is the doubled slash in `docs//zhtw:disable-block`
/// nor a third slash in `https:///zhtw:ignore`.  `x();// zhtw:ignore` still
/// works, because a comment opener is never preceded by a letter, a digit,
/// a further slash, or the `:` of a scheme.  For `#` it is the YAML and
/// shell rule, so `value# zhtw:ignore` stays data.
///
/// `hash` is false for the Markdown content types, where `#` opens a
/// heading instead of a comment.
fn opens_comment(head: &str, hash: bool) -> bool {
    if head.ends_with("<!--") {
        return true;
    }
    if let Some(stem) = head.strip_suffix("//") {
        return !stem.ends_with(|c: char| c.is_alphanumeric() || c == ':' || c == '/');
    }
    let stem = head.trim_end_matches('#');
    hash && head.ends_with('#') && (stem.is_empty() || stem.ends_with(char::is_whitespace))
}

/// Keywords accepted after "zhtw:".  Ordered so that no entry is a prefix
/// of a later one, since the first match wins; `keyword_order_is_safe`
/// enforces that.
const KEYWORDS: [(&str, Marker); 13] = [
    ("disable-next-line", Marker::NextLine),
    ("ignore-next-line", Marker::NextLine),
    ("disable-block", Marker::BlockStart),
    ("disable-next", Marker::NextLine),
    ("disable-line", Marker::Line),
    ("ignore-block", Marker::BlockStart),
    ("ignore-next", Marker::NextLine),
    ("ignore-line", Marker::Line),
    ("end-disable", Marker::BlockEnd),
    ("end-ignore", Marker::BlockEnd),
    ("disable", Marker::Line),
    ("enable", Marker::BlockEnd),
    ("ignore", Marker::Line),
];

/// Classify the first suppression marker on a trimmed line, if any.
///
/// A "zhtw:" token only counts when a comment opener immediately precedes
/// it, so prose that merely mentions a pragma is not itself a pragma.  An
/// unrecognized keyword yields None rather than falling back to a broader
/// marker: suppressing more than the user asked for is the worse failure.
fn marker_of(trimmed: &str, hash: bool) -> Option<Marker> {
    for (idx, _) in trimmed.match_indices("zhtw:") {
        if !opens_comment(trimmed[..idx].trim_end(), hash) {
            continue;
        }
        let rest = &trimmed[idx + "zhtw:".len()..];
        for (keyword, marker) in KEYWORDS {
            let Some(tail) = rest.strip_prefix(keyword) else {
                continue;
            };

            // A keyword only counts when the word ends here, so that
            // "zhtw:ignore-everything", "zhtw:ignore_rule", and
            // "zhtw:ignore-block範例" stay unknown rather than decaying into a
            // bare "zhtw:ignore". The alphanumeric test is Unicode-aware on
            // purpose: a CJK suffix ends no word.
            if !tail.starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_') {
                return Some(marker);
            }
        }
    }
    None
}

/// Scan text for suppression markers and return byte ranges to exclude.
///
/// `hash_comments` comes from the content type: see
/// [`ContentType::hash_comments`](crate::engine::scan::ContentType::hash_comments).
pub fn build_suppression_ranges(text: &str, hash_comments: bool) -> Vec<ByteRange> {
    let mut ranges = Vec::new();

    // Some(start) while a block is open.
    let mut block_start: Option<usize> = None;

    for (line_start, line) in LineIter::new(text) {
        let line_end = line_start + line.len();
        let marker = marker_of(line.trim(), hash_comments);

        // Inside a block, only the closing marker means anything.
        if let Some(start) = block_start {
            if matches!(marker, Some(Marker::BlockEnd)) {
                // Suppress from block_start to end of this line (inclusive).
                ranges.push(ByteRange {
                    start,
                    end: line_end,
                });
                block_start = None;
            }
            continue;
        }

        match marker {
            Some(Marker::BlockStart) => block_start = Some(line_start),
            Some(Marker::Line) => ranges.push(ByteRange {
                start: line_start,
                end: line_end,
            }),
            // Nothing follows the marker line: nothing to suppress.
            Some(Marker::NextLine) if line_end < text.len() => {
                let next_line_end = text[line_end..]
                    .find('\n')
                    .map(|pos| line_end + pos + 1)
                    .unwrap_or(text.len());
                ranges.push(ByteRange {
                    start: line_end,
                    end: next_line_end,
                });
            }
            _ => {}
        }
    }

    // Unclosed block: suppress from block_start to end of text.
    if let Some(start) = block_start {
        ranges.push(ByteRange {
            start,
            end: text.len(),
        });
    }

    ranges
}

/// Iterator over lines in a string, yielding (byte_start, line_text) pairs.
/// Line text includes the trailing newline if present.
struct LineIter<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> LineIter<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }
}

impl<'a> Iterator for LineIter<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.text.len() {
            return None;
        }
        let start = self.pos;
        let rest = &self.text[start..];
        let line_len = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        self.pos = start + line_len;
        Some((start, &self.text[start..self.pos]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_next_line_markdown() {
        let text = "第一行\n<!-- zhtw:ignore-next-line -->\n這行被忽略\n第四行\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        // The suppressed line is "這行被忽略\n"
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("這行被忽略"));
    }

    #[test]
    fn ignore_next_line_at_end() {
        // Suppress marker on last line with no following line.
        let text = "第一行\n<!-- zhtw:ignore-next-line -->";
        let ranges = build_suppression_ranges(text, true);
        assert!(ranges.is_empty()); // No next line to suppress.
    }

    #[test]
    fn ignore_block_markdown() {
        let text =
            "開始\n<!-- zhtw:ignore-block -->\n被忽略1\n被忽略2\n<!-- zhtw:end-ignore -->\n結束\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略1"));
        assert!(suppressed.contains("被忽略2"));
        assert!(!suppressed.contains("結束"));
    }

    #[test]
    fn ignore_block_unclosed() {
        let text = "開始\n<!-- zhtw:ignore-block -->\n被忽略到結尾";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].end, text.len());
    }

    #[test]
    fn inline_code_suppression() {
        let text = "正常行\n被忽略 // zhtw:ignore\n正常行\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略"));
    }

    #[test]
    fn no_suppression_markers() {
        let text = "這是正常文字\n沒有任何忽略標記\n";
        let ranges = build_suppression_ranges(text, true);
        assert!(ranges.is_empty());
    }

    #[test]
    fn multiple_suppressions() {
        let text = "行1\n<!-- zhtw:ignore-next-line -->\n忽略2\n行3 // zhtw:ignore\n行4\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn empty_text() {
        let ranges = build_suppression_ranges("", true);
        assert!(ranges.is_empty());
    }

    // disable alias tests

    #[test]
    fn disable_next_line_alias() {
        let text = "第一行\n<!-- zhtw:disable-next-line -->\n這行被忽略\n第四行\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("這行被忽略"));
    }

    #[test]
    fn disable_block_alias() {
        let text =
            "開始\n<!-- zhtw:disable-block -->\n被忽略1\n被忽略2\n<!-- zhtw:end-disable -->\n結束\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略1"));
        assert!(suppressed.contains("被忽略2"));
        assert!(!suppressed.contains("結束"));
    }

    #[test]
    fn end_disable_closes_ignore_block() {
        // end-disable should close ignore-block and vice-versa.
        let text = "開始\n<!-- zhtw:ignore-block -->\n被忽略1\n<!-- zhtw:end-disable -->\n結束\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略1"));
        assert!(!suppressed.contains("結束"));
    }

    #[test]
    fn inline_code_disable_alias() {
        // // zhtw:disable should work identically to // zhtw:ignore for inline
        // code.
        let text = "正常行\n被忽略 // zhtw:disable\n正常行\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略"));
    }

    // hash-comment markers (YAML, TOML, Python, shell)

    #[test]
    fn hash_comment_line_suppression() {
        let text = "name: 正常\ntitle: 被忽略  # zhtw:ignore\nname: 正常\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略"));
        // Suppression starts at the line start, not at the comment.
        assert!(suppressed.starts_with("title:"));
    }

    #[test]
    fn hash_comment_next_line_suppression() {
        let text = "# zhtw:disable-next-line\ntitle: 被忽略\ntitle: 正常\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略"));
        assert!(!suppressed.contains("正常"));
    }

    #[test]
    fn hash_comment_block_fences_a_list() {
        // The fixture-array case: fence off a region of a YAML file.
        let text = "\
terms:
# zhtw:disable-block
  - 被忽略1
  - 被忽略2
# zhtw:enable
  - 結束
";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略1"));
        assert!(suppressed.contains("被忽略2"));
        assert!(!suppressed.contains("結束"));
    }

    #[test]
    fn marker_needs_a_comment_opener() {
        // Prose that merely mentions a pragma is not a pragma. Both the bare
        // form and one quoted inside running text stay inert.
        let text = "說明 zhtw:ignore 的用法\n這行提到 zhtw:disable-block 但不是標記\n";
        assert!(build_suppression_ranges(text, true).is_empty());
    }

    #[test]
    fn hash_marker_needs_its_own_token() {
        // "value# zhtw:ignore" is data in YAML and shell, not a comment.
        let text = "url: http://example.com/a#zhtw:ignore\nkey: 值# zhtw:ignore\n";
        assert!(build_suppression_ranges(text, true).is_empty());
    }

    #[test]
    fn slash_marker_needs_its_own_token() {
        // A URL scheme separator and a doubled path slash are not comment
        // openers, however much they look like one to `ends_with`.
        for text in [
            "參考 https://zhtw:ignore 的說明\n",
            "路徑 docs//zhtw:disable-block 之下\n",
            "壞網址 https:///zhtw:ignore 也不算\n",
        ] {
            assert!(
                build_suppression_ranges(text, true).is_empty(),
                "must not be a pragma: {text}"
            );
        }

        // A comment opener after code keeps working, with or without a space in
        // front of it.
        for text in ["x();// zhtw:ignore\n", "x(); // zhtw:ignore\n"] {
            assert_eq!(
                build_suppression_ranges(text, true).len(),
                1,
                "must stay a pragma: {text}"
            );
        }
    }

    #[test]
    fn cjk_suffix_leaves_the_keyword_unknown() {
        // 範例 ends no word, so this is not "ignore-block" plus a comment.
        let text = "# zhtw:ignore-block範例\n被檢查\n";
        assert!(build_suppression_ranges(text, true).is_empty());
    }

    #[test]
    fn markdown_headings_are_not_pragmas() {
        // With hash comments off, a heading documenting a pragma is prose.
        let text = "# zhtw:ignore-block\n被檢查\n# zhtw:end-ignore\n";
        assert!(build_suppression_ranges(text, false).is_empty());
        // The same text in a hash-comment format does suppress.
        assert_eq!(build_suppression_ranges(text, true).len(), 1);
    }

    #[test]
    fn unknown_keyword_suppresses_nothing() {
        // Neither suffix may degrade into a bare "ignore".
        for text in [
            "被忽略 # zhtw:ignore-everything\n",
            "被忽略 # zhtw:ignore_rule\n",
        ] {
            assert!(
                build_suppression_ranges(text, true).is_empty(),
                "unknown keyword must not suppress: {text}"
            );
        }
    }

    #[test]
    fn crlf_line_endings() {
        let text = "行1\r\n被忽略 # zhtw:ignore\r\n行3\r\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略"));
        assert!(!suppressed.contains("行3"));
    }

    #[test]
    fn keyword_order_is_safe() {
        // First match wins, so no keyword may be a prefix of a later one.
        for (i, (earlier, _)) in KEYWORDS.iter().enumerate() {
            for (later, _) in &KEYWORDS[i + 1..] {
                assert!(
                    !later.starts_with(earlier),
                    "{earlier} shadows {later}; move {later} earlier in KEYWORDS"
                );
            }
        }
    }

    #[test]
    fn end_ignore_closes_disable_block() {
        // Reciprocal of end_disable_closes_ignore_block: end-ignore should
        // close a block opened by disable-block.
        let text = "開始\n<!-- zhtw:disable-block -->\n被忽略1\n<!-- zhtw:end-ignore -->\n結束\n";
        let ranges = build_suppression_ranges(text, true);
        assert_eq!(ranges.len(), 1);
        let suppressed = &text[ranges[0].start..ranges[0].end];
        assert!(suppressed.contains("被忽略1"));
        assert!(!suppressed.contains("結束"));
    }
}
