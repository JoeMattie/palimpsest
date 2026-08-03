# palimpsest

`pal` reads a git repo's entire history and builds a queryable database of
how files are actually related - including relationships that no longer
exist in the source but still govern how the code changes.

> *Palimpsest: a manuscript scraped clean and rewritten, where the earlier
> text remains faintly legible underneath.*

One binary (`pal`), Rust workspace, SQLite on disk at `.pal/index.db`.

## Why

There are three signals for "what else does this file touch," in increasing
order of how hard they are to get any other way:

1. **Live structural edges.** `a.ts` imports `b.ts` at HEAD. Exact, cheap,
   and any LSP will tell you the same thing. It's here because it anchors
   everything else, not because it's novel.
2. **Evolutionary coupling.** `parser.rs` and `fixtures/golden/` have no
   static link and change together in 71% of commits touching either.
   Static analysis can't see this, by construction.
3. **Ghost edges.** `encoder.ts` imported `frame.ts` until a commit
   extracted an interface between them. The import is gone. They've
   co-changed 9 times since. The contract survived; only the analyzer's
   ability to see it died.

So, here's the thing: a static edge disappearing rarely means the coupling
ended. It usually means the coupling went dark. Interface extraction,
dependency inversion, event buses, plugin registries, config-string
dispatch - every one of these preserves the contract while deleting the
evidence. That makes the severance commit a detector for exactly the class
of dependency that HEAD-time tooling can't find. And the way you tell
"refactored but still coupled" from "genuinely decoupled" is whether the
co-change kept going after the edge died (neither signal can answer that
alone, which is why the tool tracks both).

## Install and use

```
npx github:JoeMattie/palimpsest --help
```

That runs `pal` through npx: a prebuilt binary for your platform (Linux
x64/arm64, macOS x64/arm64, Windows x64) gets downloaded from the matching
GitHub release on first run and cached. npm 12+ disables git dependencies
by default, so you'll need `--allow-git=all` before the package name. Or
install with cargo:

```
cargo install --git https://github.com/JoeMattie/palimpsest pal-cli
```

That one needs a Rust toolchain. From a clone, `cargo build --release`
leaves the same binary at `target/release/pal`.

```
pal index /path/to/repo        # writes .pal/index.db in the repo
pal blast src/encoder.ts       # ranked "what else is affected"
```

Commands:

```
pal index [PATH] [--since <rev|YYYY-MM-DD>] [--incremental]
          [--max-commit-files 50] [--half-life 365d] [--all-parents]
          [--jobs N] [--quiet]
pal blast <FILE> [--depth 2] [--limit 20] [--min-confidence 0.2]
          [--kinds live,ghost,cochange,doc,transitive]
pal ghosts [FILE]           # severed edges that still co-change
pal cochange <FILE>         # evolutionary coupling, no structural edge
pal path <A> <B> [--as-of <rev|date>]
pal why <A> <B>             # severing commit + full evidence chain
pal timeline <FILE>         # birth, renames, edge events, churn
pal drift                   # stale docs
pal hotspots [--by churn|coupling|ghosts]
pal search <QUERY>          # FTS5 over commit messages
pal stats                   # index health
pal export [--format json|dot|graphml]
```

Every command takes `--json` for a stable, versioned, compact schema with a
`caveats` array (staleness warnings and the like), and `--db` to point at
an index somewhere else. Paths are repo-relative, and historical paths work
too - they resolve through the rename chain.

Exit codes: `0` ok, `2` db missing, `3` file not found in history, `4`
schema version mismatch.

## How it works

- **History walk** (git2, isolated behind a `Vcs` trait so a gix backend
  can be swapped in later): first-parent by default, oldest to newest,
  rename detection on. File identity survives rename chains and
  delete-then-re-add (blob-identical, or within a 90-day window).
- **Parsing**: tree-sitter, one file at a time, no build environment
  needed - historical checkouts aren't buildable, so anything that needs a
  build was never an option. Parses are cached by blob oid; the same
  `README.md` blob in a thousand commits parses once, ever. Languages:
  TypeScript/TSX, JavaScript, Python, Rust, Go, and Ruby, plus Markdown
  link and token extraction, and a line-based CoffeeScript extractor (no
  maintained tree-sitter grammar exists for it). Adding a language means a
  query file under `grammars/` and a resolver arm.
- **Resolution ladder** (most exact first): relative path, config-aware
  roots (go.mod module, Cargo workspace member names, ESM `.js` to `.ts`
  mapping), unique symbol name, doc refs. Ambiguity drops the edge rather
  than guessing, and each edge records its resolution quality.
- **Commit classification**: commits touching more than `max-commit-files`
  files are excluded from co-change; import-only churn (barrel reshuffles,
  alias migrations) is detected per-file and per-commit; mechanical commits
  can't create ghosts but do update the live edge set. Every decision is
  recorded as a flag, so different thresholds can be re-derived without
  reindexing from scratch.
- **Co-change**: commit weight `1/n_files`, recency decay with a one-year
  half-life, and lift over chance rather than raw counts - that last one is
  what keeps `package.json` from topping every query.
- **Ghosts**: an edge whose latest interval closed (not by a mechanical
  commit), both endpoints still alive, that lived at least 90 days, and
  whose endpoints co-changed at least twice since with confidence of at
  least 0.25. The post-severance condition is a gate, not a score term.

## Validation

Integration tests build fixture repos exercising rename chains,
delete/re-add resurrection, the canonical interface-extraction ghost, and a
lint-storm commit (`cargo test`). Observed on real repos:

| repo | commits | files | index time | resolved internal imports |
|---|---|---|---|---|
| a Python application | 412 | 101 | ~10s | 88% |
| a TypeScript monorepo | 222 | 902 | ~10s | 68% |

There's a back-testing protocol designed but not yet automated: hold out
six months of history, then measure precision@5, recall@10, and MRR of
`blast` against the co-commits that actually happened, versus
live-graph-only, raw-count, and same-directory baselines.

## Not built yet

- `pal serve` and the visualization views (strata, scrubber, archaeology,
  DSM, drift).
- Embeddings and semantic `pal search`. The tool is fully useful without
  them, on purpose.
- tsconfig `paths` aliases in the resolver (imports through `@/...` aliases
  only resolve when they happen to match a repo path).

## Layout

```
crates/
  pal-core      domain types, metric math, no I/O
  pal-store     SQLite schema and typed queries
  pal-index     git walk, tree-sitter extraction, edge resolution
  pal-analyze   co-change, ghost detection, query layer
  pal-cli       the pal binary: parsing, serialization, exit codes only
grammars/       tree-sitter query files, one dir per language
skills/palimpsest/SKILL.md   agent instructions
```
