# Third-party notices

What this repository redistributes, and under what terms. Two categories, and
they carry different obligations:

- Compiled into the released binary. Attribution travels with every download.
- Derived text in the repository. Attribution travels with the source only.

`externals/` is vendored reference material for development. It is not built,
not tested, and not redistributed, so it is not listed here.

## Compiled into the binary

### OpenCC conversion dictionaries

- Project: [OpenCC](https://github.com/BYVoid/OpenCC) by BYVoid and contributors
- License: Apache License 2.0
- Commit: `5249273a3e5606852f088c9a8b23522145d94f78`
- Files used: `data/dictionary/STPhrases.txt`, `STCharacters.txt`,
  `TWVariants.txt`. `TWPhrases*.txt` is deliberately not compiled in: those
  mappings are vocabulary decisions, which the ruleset makes instead.
- How it reaches the binary: `scripts/gen-s2t-tables.py` compiles the
  dictionaries into `src/engine/s2t_data.rs`, which is a generated file and is
  not tracked in git. The release binary therefore contains a transformed copy
  of the dictionary data.

Apache-2.0 requires that the license and attribution accompany distribution.
The commit is pinned in `[package.metadata.opencc]` in `Cargo.toml` and
verified against the `source-hash` recorded there, so the compiled tables are
reproducible.

## Derived text in this repository

### Test fixtures under `tests/fixtures/`

Small local paraphrases written for regression tests, not copies. Each
directory's `README.md` records its upstream source and pinned commit.

| Directory | Upstream | License | Commit |
|---|---|---|---|
| `humanize/` | [tzengyuxio/skills](https://github.com/tzengyuxio/skills) `humanize/SKILL.md` | MIT | `4cfb6da8a081f813314df07a6d26d260c0a6a39b` |
| `translationese/` | [tzengyuxio/skills](https://github.com/tzengyuxio/skills) `dewesternise/SKILL.md` | MIT | `4cfb6da8a081f813314df07a6d26d260c0a6a39b` |
| `writing_humanizer/` | [shyuan/writing-humanizer](https://github.com/shyuan/writing-humanizer) | MIT | `b8cb8a54962ca8a4ac589869c1ccdc3d7b74e0d1` |
| `calque/` | [Cuimao777/cuimao-translator](https://github.com/Cuimao777/cuimao-translator) | MIT | `e734bd3700f03009c9110226ad0ba0840a2342f9` |

The `calque/` fixtures are text written for this repository, but the six-red-flag
taxonomy and the EN to ZH glossary they encode are derived from
cuimao-translator, as `tests/fixtures/calque/README.md` records and commit
`417e11d` states. That is an attribution obligation, so it belongs here rather
than only in the commit message.

### Terminology

Rule targets in `assets/ruleset.json` follow published Taiwan usage:
教育部重編國語辭典, 國家教育研究院 樂詞網, and 內政部國土測繪中心. Individual
term pairs are facts about language rather than copyrightable expression; each
rule's `context` field records which source it follows.
