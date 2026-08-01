# palimpsest

A tool that reads a git repo's entire history and builds a queryable
database of how files are actually related, including relationships that no
longer exist in the source but still govern how the code changes.

> *Palimpsest: a manuscript scraped clean and rewritten, where the earlier
> text remains faintly legible underneath.*

Binary: `pal`. Rust workspace, SQLite on disk at `.pal/index.db`.

## Why

Three signals about "what else does this file touch," in increasing order
of how hard they are to get any other way:

1. **Live structural edges.** `a.ts` imports `b.ts` at HEAD. Exact, cheap,
   already available from any LSP. Included because it anchors everything
   else.
2. **Evolutionary coupling.** `parser.rs` and `fixtures/golden/` have no
   static link and change together in 71% of commits touching either.
   Invisible to static analysis by construction.
3. **Ghost edges.** `encoder.ts` imported `frame.ts` until a commit
   extracted an interface between them. The import is gone. They have
   co-changed 9 times since. The contract survived; only the analyzer's
   ability to see it died.

A static edge disappearing is rarely coupling ending; it is usually
coupling going dark. Interface extraction, dependency inversion, event
buses, plugin registries, config-string dispatch: every one preserves the
contract while deleting the evidence. The severance commit is therefore a
detector for exactly the class of dependency that HEAD-time tooling cannot
find. The discriminator between "refactored but still coupled" and
"genuinely decoupled" is whether co-change persisted after the edge died;
neither signal alone can answer that.

## Install and use

```
cargo install --git https://github.com/JoeMattie/palimpsest pal-cli
```

Installs the `pal` binary. Requires a Rust toolchain; from a clone,
`cargo build --release` leaves the same binary at `target/release/pal`.

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
an index elsewhere. Paths are repo-relative; historical paths are accepted
and resolved through the rename chain.

Exit codes: `0` ok, `2` db missing, `3` file not found in history, `4`
schema version mismatch.

## How it works

- **History walk** (git2, isolated behind a `Vcs` trait so a gix backend
  can be swapped in): first-parent by default, oldest to newest, rename
  detection on. File identity survives rename chains and delete-then-re-add
  (blob-identical or within a 90-day window).
- **Parsing**: tree-sitter, one file at a time, no build environment
  needed, because historical checkouts are not buildable. Parses are cached
  by blob oid; the same `README.md` blob in a thousand commits parses once,
  ever. Languages: TypeScript/TSX, JavaScript, Python, Rust, Go, Ruby, plus
  Markdown link and token extraction, and a line-based CoffeeScript
  extractor (no maintained tree-sitter grammar exists for it). Adding a
  language means adding a query file under `grammars/` and a resolver arm.
- **Resolution ladder** (most exact first): relative path, config-aware
  roots (go.mod module, Cargo workspace member names, ESM `.js` to `.ts`
  mapping), unique symbol name (ambiguity drops the edge rather than
  guessing), doc refs. Each edge records its resolution quality.
- **Commit classification**: commits touching more than `max-commit-files`
  files are excluded from co-change; import-only churn (barrel reshuffles,
  alias migrations) is detected per-file and per-commit; mechanical commits
  may not create ghosts but do update the live edge set. All decisions are
  recorded as flags so different thresholds can be re-derived.
- **Co-change**: commit weight `1/n_files`, recency decay with a one-year
  half-life, and lift over chance rather than raw counts, so `package.json`
  does not top every query.
- **Ghosts**: an edge whose latest interval closed, not by a mechanical
  commit, both endpoints alive, that lived at least 90 days, and whose
  endpoints co-changed at least twice since with confidence at least 0.25.
  The post-severance condition is a gate, not a score term.

## Validation

Integration tests build fixture repos exercising rename chains,
delete/re-add resurrection, the canonical interface-extraction ghost, and a
lint-storm commit (`cargo test`). Observed on real repos:

| repo | commits | files | index time | resolved internal imports |
|---|---|---|---|---|
| Deep-Live-Cam (Python) | 412 | 101 | ~10s | 88% |
| authorbot (TS monorepo) | 222 | 902 | ~10s | 68% |

The back-testing protocol from the plan (hold out six months, measure
precision@5 / recall@10 / MRR of `blast` against actual future co-commits,
versus live-graph-only, raw-count, and same-directory baselines) is
specified in `PALIMPSEST_PLAN.md` section 9 and not yet automated.

## Not built yet

- `pal serve` and the `pal-viz` views (strata, scrubber, archaeology, DSM,
  drift): plan phase P6.
- Embeddings and semantic `pal search` (`fastembed` feature): plan P7. The
  tool is fully useful without them by design.
- tsconfig `paths` aliases in the resolver (bare-specifier imports through
  `@/...` aliases resolve only when they happen to match a repo path).

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
