# Rule types

Spelling rules and case rules, organized into 8 categories.

## cross_strait

Regional terminology differences between zh-CN and zh-TW. Each rule has an `english` field for disambiguation.

| zh-CN | zh-TW | English |
|-------|-------|---------|
| 軟件 | 軟體 | software |
| 內存 | 記憶體 | memory (RAM) |
| 線程 | 執行緒 | thread |
| 進程 | 行程 | process |
| 接口 | 介面 | interface |
| 人工智能 | 人工智慧 | artificial intelligence |
| 操作系統 | 作業系統 | operating system |
| 默認 | 預設 | default |
| 代碼 | 程式碼 | code |
| \u201c / \u201d | 「 / 」 | quotation marks |

Quotation mark conversion includes a pairing fix: when CN curly quotes are unbalanced or misordered, the scanner reassigns them by alternating position (open, close, open, close).

Some cross-strait rules involve false friends (假朋友), where the `from` term is also a valid zh-TW word with a different meaning. For example, 文件 means "file" in zh-CN but "document" in zh-TW. These rules are disabled to prevent false positives.

A milder case gets the optional `editorial_confidence` field instead of being disabled, and a term needing different corrections in different domains gets `context_suggestions`. Both are described under [Optional rule fields](#optional-rule-fields).

Tree data structure terminology follows a gender-neutral naming principle (性別中立原則). English terms like "parent" and "sibling" are inherently gender-neutral, so zh-TW translations should preserve that neutrality rather than importing gendered kinship terms:

| Flagged | Suggested | English | Rationale |
|---------|-----------|---------|-----------|
| 父節點 | 親代節點 | parent node | 「親代」preserves the gender-neutral semantics of "parent" |
| 母節點 | 親代節點 | parent node | Every non-root node has exactly one parent, not a gendered pair |
| 兄弟節點 | 平輩節點 | sibling node | 「平輩」expresses same-level kinship without gender |
| 叔伯節點 | 親代的平輩節點 | uncle node | Compositional form avoids gendered kinship metaphors |

## punctuation

Context-sensitive half-width to full-width punctuation normalization for Chinese text:

| Half-width | Full-width | Condition |
|------------|------------|-----------|
| `,` | `，` | Adjacent CJK character on either side |
| `.` | `。` | Preceding CJK character (guards against decimals, file extensions, ellipsis) |
| `!` | `！` | Adjacent CJK character |
| `?` | `？` | Adjacent CJK character |
| `;` | `；` | Adjacent CJK character |
| `:` | `：` | Adjacent CJK character (exempted with `relaxed` flag) |
| `(` / `)` | `（` / `）` | Adjacent CJK character |

Also detects: CN curly quotation marks (`\u201c`/`\u201d` double, `\u2018`/`\u2019` single) with CJK adjacency guards to avoid false positives on English smart quotes and contractions (it's, don't); enumeration comma misuse (`，` where `、` is appropriate for coordinate lists); quotation mark hierarchy violations; extraneous space after full-width punctuation; and range indicator style (`～` vs `–`).

English-only contexts, thousand separators (1,000), and decimal numbers (3.14) are left untouched.

## political_coloring

Terms carrying political framing inappropriate for Taiwan contexts.

| Flagged | Suggested | English |
|---------|-----------|---------|
| 祖國 | 中國 | motherland |
| 內地 | 中國大陸 / 中國 | mainland |
| 大陸同胞 | 中國民眾 | mainland compatriots |

## confusable

Terms that are easily confused across dialects.

| Flagged | Suggested | English | Note |
|---------|-----------|---------|------|
| 字體 | 字型 | font | 字體 = typeface (design family); 字型 = font (specific size/weight instance) |

## typo

Common misspellings.

| Flagged | Suggested | English |
|---------|-----------|---------|
| 乞業 | 企業 | enterprise |

## variant

Character variant normalization per the MoE Standard Form of National Characters (國字標準字體). These map non-standard glyph forms (Kangxi, Hong Kong, generic zh-Hant) to the Taiwan standard:

| Non-standard | MoE standard | Notes |
|-------------|-------------|-------|
| 裏 | 裡 | "inside" |
| 綫 | 線 | "thread/line" |
| 麪 | 麵 | "noodle" |
| 着 | 著 | Particle usage; exception: chess term 下著, proper nouns |
| 台 | 臺 | `strict` profile only; lexical contexts: 臺灣/臺北/臺中/臺南 |

Variant rules use a separate engine pass (after spelling rules) with exception phrase checking.

## proper_noun

Country names and international organizations with cross-strait naming differences:

| zh-CN | zh-TW | English |
|-------|-------|---------|
| 老撾 | 寮國 | Laos |
| 新西蘭 | 紐西蘭 | New Zealand |
| 東盟 | 東協 | ASEAN |

## case

Proper casing for technology terms. Matched case-insensitively with word boundary checks.

```
JavaScript  TypeScript  Python  Rust  HTTP  HTTPS
API  JSON  GitHub  Instagram  Google  Facebook
React  Linux  macOS
```

## Optional rule fields

These apply to any lexical rule type, not just `cross_strait`. Seven rules currently carry `editorial_confidence`, and five of them are not `cross_strait`: one `confusable` and four `translationese`.

### editorial_confidence

`"low"` marks a rule whose flagged form is valid zh-TW and whose suggestion is a register preference rather than a correction, so the term is worth reporting but not worth rewriting unattended. Auto-fix honors it: `lexical_safe` declines these rules and only `lexical_contextual` applies them.

Use it sparingly; a rule that is simply wrong in zh-TW should carry no annotation, even when the flagged form has a valid unrelated sense. 算法 is the reference case: 演算法 is the MoE standard, so the rule stays unannotated and auto-fixes, and the arithmetic sense of 算法 is handled with `context_clues` if it ever needs handling.

Only lexical rule types can carry the field. The fixer's gate is guarded on lexical issues, and `variant` rules classify as orthographic, so an annotation there would be silently ignored; `scripts/check-ruleset.py --lint` rejects that placement.

The MCP `explain` output also reports `auto_fix_safe` and `needs_review`, but on a wider notion of low confidence: when a rule carries no annotation it falls back to a heuristic that treats translationese, AI-style, grammar, `Info`-severity, and anchor-rejected issues as low. That fallback decides what to tell a human reviewer, not what the fixer writes. Do not read `auto_fix_safe: false` as a prediction that `--fix=lexical_safe` will decline the issue; only the explicit ruleset annotation gates the fixer.

### context_suggestions

One source term can need different corrections in different domains, and a flat `to` list cannot say so. `context_suggestions` is a list of `{clues, to}` groups: when any clue appears in the same ±40-character window the context-clue gate uses (clamped at paragraph breaks and at excluded ranges such as code blocks, so a clue in the next paragraph or inside a fence cannot select a group), that group's `to` replaces the rule's default for that match only. Groups are tried in order, so the first match wins and ruleset order is the precedence order.

`優化` is the worked example. IT `optimize` takes 最佳化, but where the text means improve rather than make-optimal, 「優化」is a misuse and the right word is 改善 or 提升, per <https://hackmd.io/@sysprog/it-vocabulary>:

```json
{
  "from": "優化",
  "to": ["最佳化"],
  "context_suggestions": [
    { "clues": ["微服務", "服務端", "客戶端", "用戶端"], "to": ["最佳化"] },
    { "clues": ["流程", "體驗", "服務", "客戶", "顧客", "營運", "績效", "品質"], "to": ["改善", "提升"] }
  ]
}
```

So 「優化演算法」suggests 最佳化 and auto-fixes, while 「優化客戶服務流程」suggests 改善 or 提升 and does not. That difference is deliberate: a group carrying several entries is never auto-applied at any tier, because choosing between them is a judgment call. Putting 改善 and 提升 directly in `to` would instead disable auto-fix for the IT sense as well, which is the trade this field exists to avoid.

That invariant is structural rather than per-field: the fixer writes only when exactly one candidate is on offer, whatever produced it and whatever the tier. Groups are still dropped at compile time on deletion rules, for an unrelated reason: the reported span comes from the rule's own `to`, so a group offering a real replacement would report a shorter span than it rewrites.

Selection is a raw substring test over the window, so a clue matches inside
longer words: 服務 matches 微服務, 客戶 matches 客戶端. Dropping those clues is
the wrong fix, because it drops the bare business reading with them, and there
the rule default is not merely unhelpful but wrong. 「優化服務」would fall through
to 最佳化 and auto-fix at `lexical_safe`, silently rewriting a sentence that
means improve the service, where before this field existed the term was not
flagged there at all.

The narrow IT group above repeats the rule default 最佳化 rather than offering
anything new, because its only job is to claim those compounds before the broad
group's 服務 and 客戶 match inside them. `--lint` rejects the two groups in the
other order, since the broad one would swallow the narrow one entirely.

A clue this field gets wrong is worse than one `context_clues` gets wrong: it
does not just gate the match, it replaces the suggestion, and a multi-entry
group also removes auto-fix at every tier.

A malformed group is dropped whole, never repaired. That covers an empty `clues` list (can never select), an empty `to` list (would erase the rule default), and an empty string anywhere inside `to`. The last one matters most: filtering the empty entry out of `["改善", ""]` would leave a one-entry group, and one entry is auto-fixable, so a typo would quietly grant the write permission the author's two candidates were meant to deny. A clue that also appears in `negative_context_clues` can never select, because the negative clue vetoes the whole match first; `--lint` warns about all of these.

## Extending the ruleset

### Adding a spelling rule

Edit `assets/ruleset.json`:

```json
{
  "from": "數據庫",
  "to": ["資料庫"],
  "type": "cross_strait",
  "context": "database = 資料庫",
  "english": "database"
}
```

Run `scripts/check-ruleset.py --lint` to validate before opening a PR.

Fields: `from` (required), `to` (required, array), `type` (required: `cross_strait` / `political_coloring` / `confusable` / `typo` / `variant`), `disabled` (optional), `context` (optional, use `@seealso` for cross-refs), `english` (optional, recommended).

### Adding a case rule

```json
{
  "term": "GraphQL",
  "alternatives": ["graphql", "GRAPHQL", "Graphql"]
}
```

### Runtime overrides

Edit `overrides.json` in the platform config directory (`~/.config/zhtw-mcp/` on Linux, `~/Library/Application Support/zhtw-mcp/` on macOS):

```json
{
  "schema_version": 3,
  "spelling": [
    {"from": "優化", "to": ["最佳化"], "type": "cross_strait", "disabled": true}
  ],
  "case": []
}
```
