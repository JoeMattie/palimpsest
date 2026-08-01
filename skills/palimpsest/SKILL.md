---
name: palimpsest
description: Query a repo's palimpsest index (pal) before editing files. Use when a repository contains .pal/index.db and you are about to modify code, assess change impact, or investigate why two files move together. Surfaces co-change coupling and ghost edges (deleted imports that still bite) that static analysis cannot see.
---

# palimpsest: history-aware impact analysis

`pal` reads a prebuilt index of the repo's entire git history and answers
"what else moves when this file moves," including relationships that no
longer exist in the source. Three kinds of signal, in increasing order of
how hard they are to get anywhere else:

1. **Live structural edges.** Imports, calls, type references at HEAD. Any
   LSP can tell you this; pal includes it as an anchor.
2. **Evolutionary coupling.** Files that change together far more than
   chance, with no structural link. Fixture dirs, config, golden files,
   protocol peers. Invisible to static analysis by construction.
3. **Ghost edges.** A structural edge that was severed (interface
   extraction, dependency inversion, event bus, config dispatch) while the
   files kept co-changing afterward. The contract survived; only the
   analyzer's ability to see it died. This is the signal you cannot get
   from HEAD-time tooling at all, and it is where unexpected breakage
   lives.

## Trigger

Before editing any file in a repository that has a `.pal/index.db`
(check with `ls .pal/index.db` at the repo root, or just run `pal stats`;
exit code 2 means there is no index and this skill does not apply).

## Workflow

1. `pal blast <FILE> --json` on the file you are about to edit.
2. Read the top-ranked results. For each one, the `evidence` array tells
   you what kind of relationship it is; treat them differently (see below).
3. `pal why <A> <B> --json` on anything surprising, especially ghosts: it
   returns the severing commit, its message, and the full evidence chain.
   The severing commit's message usually names the mechanism (the
   interface, the event, the config key) that still binds the files.
4. Make the edit, checking the files the evidence licensed you to worry
   about.

Other commands as needed:

- `pal cochange <FILE>`: coupling with no structural edge at all (fixtures,
  goldens, config). Check these when changing behavior, not just interfaces.
- `pal ghosts [FILE]`: ghost edges repo-wide or for one file.
- `pal timeline <FILE>`: births, renames, edge history. Useful when a file's
  role seems to have shifted.
- `pal drift`: docs whose referenced code moved on without them; check
  before trusting a doc, and consider updating a drifted doc you touch.
- `pal search <QUERY>`: full-text over commit messages.
- `pal stats`: index health. Look at `unresolved_ratio` and `excluded_pct`
  to calibrate how much to trust edge-based evidence.

## Reading provenance

Every result carries evidence, never a bare score. What each type licenses:

- `structural` (alive): a compiler-visible dependency. The cheapest to
  verify; your editor would have told you too.
- `ghost`: the strongest signal in the tool. `severed_at` plus
  `severing_subject` tell you when and why the edge was cut;
  `cochanges_since` and `confidence_since` tell you the contract is still
  live. Read the severing commit before assuming the files are independent.
- `cochange`: `n` co-changes, `confidence` (P(other | this)), `lift` (how
  far above chance). Lift below ~2 with low confidence is weak; 9 co-changes
  at confidence 0.6 and lift 4 is a strong hidden contract.
- `transitive`: near in the union-of-all-time graph, far or unreachable in
  the live graph; usually an inlined or deleted intermediary. Weakest
  evidence; treat as a pointer to investigate, not a conclusion.
- `doc_drift`: a doc references this file and is N commits behind.

## Token discipline

- Always `--json` when consuming programmatically; the schema is stable and
  versioned (`"schema": 1`).
- Default `--limit` is 20 and outputs are ranked; do not raise it unless
  the truncated flag matters to the task.
- Never dump `pal export` or unfiltered graphs into context.
- `--kinds live,ghost,cochange,doc,transitive` narrows blast output when
  you only need one signal.

## Epistemics: what this tool can and cannot claim

This is a recall aid, not a proof.

- "These 9 files historically move with this one" is a supportable claim,
  and worth acting on.
- "Nothing else is affected" is a claim the index can NEVER make. Absence
  of evidence here is absence of recorded history, not evidence of absence.
  New code has no history; externally-coupled code (shared DBs, wire
  protocols, deploy configs in other repos) never had any.
- Ranks are for ordering attention, not for thresholding truth. A 0.3 with
  ghost evidence deserves more attention than a 0.5 from one noisy
  co-change burst.
- Check the `caveats` array in every response. A stale index (behind HEAD)
  silently misses the newest coupling; run `pal index --incremental` when
  it says so.
- The index reflects the walked history: squash merges collapse detail, and
  commits excluded as too-large or mechanical (see `pal stats`) contribute
  nothing to co-change. A repo with 40% excluded commits has a thinner
  evidence base than the same numbers would suggest elsewhere.

## Exit codes

0 ok; 2 no index (run `pal index`); 3 file not found in indexed history
(check the path is repo-relative; historical paths are accepted and follow
renames); 4 index schema version mismatch (re-run `pal index`).
