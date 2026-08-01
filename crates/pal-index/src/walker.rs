//! The indexing pipeline: walk history oldest to newest, maintain file
//! identity through renames, record touches, parse changed blobs (cached by
//! blob oid), resolve edges, and open or close edge intervals.

use crate::classify::{classify_commit, import_only_fraction, is_excluded_path};
use crate::langs::{self, Lang};
use crate::resolve::{resolve_file, ResolveCtx};
use crate::vcs::{Change, FileChange, GitVcs, Vcs};
use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use pal_core::metrics::Params;
use pal_core::parsed::{ParsedFile, PARSER_VERSION};
use pal_core::{ChangeKind, CommitId, EdgeKind, FileId};
use pal_store::Store;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

const RESURRECT_WINDOW_DAYS: i64 = 90;
const TX_CHUNK: usize = 500;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub repo: PathBuf,
    pub db: PathBuf,
    pub since: Option<String>,
    pub incremental: bool,
    pub all_parents: bool,
    pub params: Params,
    pub quiet: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexReport {
    pub commits_indexed: usize,
    pub files_total: i64,
    pub imports_total: u64,
    pub imports_resolved: u64,
    pub head: String,
    pub elapsed_secs: f64,
}

#[derive(Debug, Clone)]
struct DeadEntry {
    file: FileId,
    died_time: i64,
    last_blob: Option<Vec<u8>>,
    /// In-edges (src, kind, edge_id) closed by this death, for reopening on
    /// delete-then-re-add histories.
    closed_in: Vec<(FileId, EdgeKind, i64)>,
}

#[derive(Default)]
struct WalkState {
    live: HashMap<String, FileId>,
    lang_of: HashMap<FileId, Lang>,
    dead: HashMap<String, DeadEntry>,
    defs_by_file: HashMap<FileId, BTreeSet<String>>,
    symbols: HashMap<String, HashSet<FileId>>,
    out_edges: HashMap<FileId, BTreeSet<(FileId, EdgeKind)>>,
    in_edges: HashMap<FileId, BTreeSet<(FileId, EdgeKind)>>,
    edge_ids: HashMap<(FileId, FileId, EdgeKind), i64>,
    go_module: Option<String>,
    rust_crates: HashMap<String, String>,
    imports_total: u64,
    imports_resolved: u64,
}

pub fn index(opts: &IndexOptions) -> Result<IndexReport> {
    let started = std::time::Instant::now();
    let vcs = GitVcs::open(&opts.repo)?;
    let head = vcs.head_oid()?;

    let fresh = !opts.db.exists();
    let store = if fresh {
        Store::create(&opts.db)?
    } else {
        Store::open(&opts.db).or_else(|_| Store::create(&opts.db))?
    };

    let prior_head = store.meta_get("head_oid").ok().flatten();
    let (hide, mut state) = if opts.incremental && !fresh {
        match &prior_head {
            Some(h) => {
                if *h == pal_store::full_oid(&head) {
                    return Ok(IndexReport {
                        commits_indexed: 0,
                        files_total: store.count("SELECT COUNT(*) FROM files")?,
                        imports_total: meta_u64(&store, "imports_total"),
                        imports_resolved: meta_u64(&store, "imports_resolved"),
                        head: pal_store::full_oid(&head),
                        elapsed_secs: started.elapsed().as_secs_f64(),
                    });
                }
                (Some(h.clone()), load_state(&store, &vcs)?)
            }
            None => bail!("--incremental needs an existing index with a recorded head"),
        }
    } else {
        if !fresh {
            bail!(
                "index already exists at {}; use --incremental to extend it or delete it to rebuild",
                opts.db.display()
            );
        }
        (opts.since.clone(), WalkState::default())
    };

    // A date-shaped --since filters by time instead of by rev ancestry.
    let (hide_rev, since_time) = match &hide {
        Some(s) if is_date(s) => (None, Some(parse_date(s)?)),
        Some(s) => (Some(s.clone()), None),
        None => (None, None),
    };

    let commits = vcs
        .commits(!opts.all_parents, hide_rev.as_deref(), since_time)
        .context("walking history")?;
    if commits.is_empty() {
        bail!("no commits to index");
    }

    let bar = if opts.quiet {
        ProgressBar::hidden()
    } else {
        let b = ProgressBar::new(commits.len() as u64);
        b.set_style(
            ProgressStyle::with_template(
                "{spinner} [{elapsed_precise}] {bar:36} {pos}/{len} commits {msg}",
            )
            .unwrap(),
        );
        b
    };

    store.begin()?;

    // Seed live state from the parent of the first commit when we are not
    // walking from the repository root (fresh --since index).
    let needs_seed = !opts.incremental && commits[0].parent_oid.is_some();
    if needs_seed {
        let parent = commits[0].parent_oid.clone().unwrap();
        seed_from_tree(&store, &vcs, &mut state, &parent, &opts.params)?;
    }

    let mut indexed = 0usize;
    for (i, info) in commits.iter().enumerate() {
        let changes = vcs.diff_commit(
            &info.oid,
            info.parent_oid.as_deref(),
            opts.params.rename_threshold,
        )?;
        let changes = normalize_changes(changes);

        let import_only: Vec<bool> = changes
            .iter()
            .map(|fc| {
                let lang = Lang::from_path(fc.change.path());
                !fc.changed_lines.is_empty()
                    && import_only_fraction(lang, &fc.changed_lines)
                        >= opts.params.import_only_ratio
            })
            .collect();

        let exclude_merges = opts.all_parents && info.is_merge;
        let class = classify_commit(
            &opts.params,
            &info.subject,
            changes.len(),
            &import_only,
            info.is_merge,
            exclude_merges,
        );

        let commit_id = store.insert_commit(
            &info.oid,
            info.parent_oid.as_deref(),
            info.author_time,
            &info.author,
            &info.subject,
            &info.body,
            changes.len() as i64,
            class.weight,
            class.excluded_flags,
            info.is_merge,
        )?;

        apply_commit(
            &store,
            &vcs,
            &mut state,
            commit_id,
            info.author_time,
            &changes,
            &import_only,
            &opts.params,
        )?;

        indexed += 1;
        bar.inc(1);
        if (i + 1) % TX_CHUNK == 0 {
            store.commit_tx()?;
            store.begin()?;
        }
    }

    store.meta_set("repo_path", &opts.repo.display().to_string())?;
    store.meta_set("head_oid", &pal_store::full_oid(&head))?;
    store.meta_set(
        "indexed_at",
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default(),
    )?;
    store.meta_set("first_parent", if opts.all_parents { "0" } else { "1" })?;
    store.meta_set(
        "max_commit_files",
        &opts.params.max_commit_files.to_string(),
    )?;
    store.meta_set("half_life_days", &opts.params.half_life_days.to_string())?;
    store.meta_set("imports_total", &state.imports_total.to_string())?;
    store.meta_set("imports_resolved", &state.imports_resolved.to_string())?;
    store.commit_tx()?;
    bar.finish_and_clear();

    Ok(IndexReport {
        commits_indexed: indexed,
        files_total: store.count("SELECT COUNT(*) FROM files")?,
        imports_total: state.imports_total,
        imports_resolved: state.imports_resolved,
        head: pal_store::full_oid(&head),
        elapsed_secs: started.elapsed().as_secs_f64(),
    })
}

fn meta_u64(store: &Store, key: &str) -> u64 {
    store
        .meta_get(key)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn is_date(s: &str) -> bool {
    s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-'
}

fn parse_date(s: &str) -> Result<i64> {
    let parts: Vec<i64> = s.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        bail!("bad date: {s} (expected YYYY-MM-DD)");
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    // Days from civil, inverse of pal_core::time::civil_from_unix.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok((era * 146_097 + doe - 719_468) * 86_400)
}

/// Drop vendored paths; downgrade renames that cross the exclusion boundary.
fn normalize_changes(changes: Vec<FileChange>) -> Vec<FileChange> {
    let mut out = Vec::with_capacity(changes.len());
    for mut fc in changes {
        match &fc.change {
            Change::Added { path } | Change::Modified { path } | Change::Deleted { path } => {
                if is_excluded_path(path) {
                    continue;
                }
            }
            Change::Renamed { from, to } => {
                let from_ex = is_excluded_path(from);
                let to_ex = is_excluded_path(to);
                if from_ex && to_ex {
                    continue;
                }
                if from_ex {
                    fc.change = Change::Added { path: to.clone() };
                } else if to_ex {
                    fc.change = Change::Deleted { path: from.clone() };
                    fc.new_blob = None;
                }
            }
        }
        out.push(fc);
    }
    // Identity updates must see deletes and renames before adds.
    out.sort_by_key(|fc| match fc.change {
        Change::Deleted { .. } => 0,
        Change::Renamed { .. } => 1,
        Change::Modified { .. } => 2,
        Change::Added { .. } => 3,
    });
    out
}

#[allow(clippy::too_many_arguments)]
fn apply_commit(
    store: &Store,
    vcs: &dyn Vcs,
    state: &mut WalkState,
    commit_id: CommitId,
    commit_time: i64,
    changes: &[FileChange],
    import_only: &[bool],
    params: &Params,
) -> Result<()> {
    // Phase A: detach current paths for files leaving their path this commit
    // so the unique index never sees a transient collision (swaps, chains).
    for fc in changes {
        match &fc.change {
            Change::Deleted { path } | Change::Renamed { from: path, .. } => {
                if let Some(id) = state.live.get(path) {
                    store.file_clear_path(*id)?;
                }
            }
            _ => {}
        }
    }

    // Phase B: apply identity changes and record touches.
    let mut touched: Vec<(FileId, &FileChange)> = Vec::new();
    for (fc, &imp_only) in changes.iter().zip(import_only) {
        match &fc.change {
            Change::Deleted { path } => {
                let Some(id) = state.live.remove(path) else {
                    continue;
                };
                let last_blob = fc.old_blob.clone();
                store.file_set_died(id, commit_id)?;
                store.symbols_close_all_for_file(id, commit_id)?;
                remove_symbols(state, id);
                let mut closed_in = Vec::new();
                close_all_edges(store, state, id, commit_id, &mut closed_in)?;
                state.lang_of.remove(&id);
                state.dead.insert(
                    path.clone(),
                    DeadEntry {
                        file: id,
                        died_time: commit_time,
                        last_blob,
                        closed_in,
                    },
                );
                store.insert_touch(
                    commit_id,
                    id,
                    ChangeKind::Deleted,
                    None,
                    Some(fc.lines_added),
                    Some(fc.lines_removed),
                    imp_only,
                )?;
            }
            Change::Renamed { from, to } => {
                let Some(id) = state.live.remove(from) else {
                    // Rename of an untracked file: treat as add below.
                    let id = add_file(store, state, to, fc, commit_id, commit_time)?;
                    touched.push((id, fc));
                    store.insert_touch(
                        commit_id,
                        id,
                        ChangeKind::Added,
                        fc.new_blob.as_deref(),
                        Some(fc.lines_added),
                        Some(fc.lines_removed),
                        imp_only,
                    )?;
                    continue;
                };
                state.live.insert(to.clone(), id);
                store.file_record_rename(id, to, commit_id, fc.confidence)?;
                store.file_set_path(id, to)?;
                let lang = Lang::from_path(to);
                store.file_set_lang(
                    id,
                    lang.map(|l| l.as_str()),
                    lang.map(|l| l.is_doc()).unwrap_or(false),
                )?;
                match lang {
                    Some(l) => {
                        state.lang_of.insert(id, l);
                    }
                    None => {
                        state.lang_of.remove(&id);
                    }
                }
                touched.push((id, fc));
                store.insert_touch(
                    commit_id,
                    id,
                    ChangeKind::Renamed,
                    fc.new_blob.as_deref(),
                    Some(fc.lines_added),
                    Some(fc.lines_removed),
                    imp_only,
                )?;
            }
            Change::Modified { path } => {
                let id = match state.live.get(path) {
                    Some(id) => *id,
                    None => add_file(store, state, path, fc, commit_id, commit_time)?,
                };
                touched.push((id, fc));
                store.insert_touch(
                    commit_id,
                    id,
                    ChangeKind::Modified,
                    fc.new_blob.as_deref(),
                    Some(fc.lines_added),
                    Some(fc.lines_removed),
                    imp_only,
                )?;
            }
            Change::Added { path } => {
                let id = add_file(store, state, path, fc, commit_id, commit_time)?;
                touched.push((id, fc));
                store.insert_touch(
                    commit_id,
                    id,
                    ChangeKind::Added,
                    fc.new_blob.as_deref(),
                    Some(fc.lines_added),
                    Some(fc.lines_removed),
                    imp_only,
                )?;
            }
        }
    }

    // Config files feed the resolver.
    for (_, fc) in &touched {
        let path = fc.change.path();
        if let Some(blob) = &fc.new_blob {
            update_configs(vcs, state, path, blob);
        }
    }

    // Parse changed files, hitting the blob cache first.
    let jobs: Vec<(FileId, String, Vec<u8>)> = {
        let mut seen = HashSet::new();
        touched
            .iter()
            .filter(|(id, _)| seen.insert(*id))
            .filter_map(|(id, fc)| {
                let blob = fc.new_blob.clone()?;
                state.lang_of.get(id)?;
                Some((*id, fc.change.path().to_string(), blob))
            })
            .collect()
    };
    let parsed = parse_blobs(store, vcs, state, jobs)?;

    // Symbols first, so co-committed files can resolve each other's names.
    for (id, _, pf) in &parsed {
        update_symbols(store, state, *id, pf, commit_id)?;
    }

    // Then edges.
    for (id, path, pf) in &parsed {
        let lang = match state.lang_of.get(id) {
            Some(l) => *l,
            None => continue,
        };
        let ctx = ResolveCtx {
            live: &state.live,
            symbols: &state.symbols,
            go_module: state.go_module.as_deref(),
            rust_crates: &state.rust_crates,
        };
        let resolved = resolve_file(&ctx, *id, path, lang, pf);
        state.imports_total += resolved.imports_total;
        state.imports_resolved += resolved.imports_resolved;
        apply_edge_diff(store, state, *id, resolved.edges, commit_id)?;
    }

    let _ = params;
    Ok(())
}

fn add_file(
    store: &Store,
    state: &mut WalkState,
    path: &str,
    fc: &FileChange,
    commit_id: CommitId,
    commit_time: i64,
) -> Result<FileId> {
    let lang = Lang::from_path(path);
    // Delete-then-re-add resurrection: same path within the window, or the
    // identical blob, resurrects the old identity.
    if let Some(entry) = state.dead.get(path) {
        let same_blob = matches!((&entry.last_blob, &fc.new_blob), (Some(a), Some(b)) if a == b);
        let within_window = commit_time - entry.died_time <= RESURRECT_WINDOW_DAYS * 86_400;
        if same_blob || within_window {
            let entry = state.dead.remove(path).unwrap();
            let id = entry.file;
            store.file_resurrect(id, path, commit_id)?;
            state.live.insert(path.to_string(), id);
            if let Some(l) = lang {
                state.lang_of.insert(id, l);
            }
            // Reopen in-edges severed by the death, for importers still alive.
            for (src, kind, edge_id) in entry.closed_in {
                if state.live.values().any(|v| *v == src) {
                    store.interval_open(edge_id, commit_id)?;
                    state.out_edges.entry(src).or_default().insert((id, kind));
                    state.in_edges.entry(id).or_default().insert((src, kind));
                    state.edge_ids.insert((src, id, kind), edge_id);
                }
            }
            return Ok(id);
        }
    }
    let id = store.insert_file(
        path,
        lang.map(|l| l.as_str()),
        commit_id,
        lang.map(|l| l.is_doc()).unwrap_or(false),
    )?;
    state.live.insert(path.to_string(), id);
    if let Some(l) = lang {
        state.lang_of.insert(id, l);
    }
    Ok(id)
}

fn remove_symbols(state: &mut WalkState, id: FileId) {
    if let Some(defs) = state.defs_by_file.remove(&id) {
        for name in defs {
            if let Some(set) = state.symbols.get_mut(&name) {
                set.remove(&id);
                if set.is_empty() {
                    state.symbols.remove(&name);
                }
            }
        }
    }
}

/// Close every open interval touching a dying file, remembering closed
/// in-edges for possible resurrection.
fn close_all_edges(
    store: &Store,
    state: &mut WalkState,
    id: FileId,
    commit_id: CommitId,
    closed_in: &mut Vec<(FileId, EdgeKind, i64)>,
) -> Result<()> {
    if let Some(outs) = state.out_edges.remove(&id) {
        for (dst, kind) in outs {
            if let Some(edge_id) = state.edge_ids.get(&(id, dst, kind)) {
                store.interval_close(*edge_id, commit_id)?;
            }
            if let Some(ins) = state.in_edges.get_mut(&dst) {
                ins.remove(&(id, kind));
            }
        }
    }
    if let Some(ins) = state.in_edges.remove(&id) {
        for (src, kind) in ins {
            if let Some(edge_id) = state.edge_ids.get(&(src, id, kind)) {
                store.interval_close(*edge_id, commit_id)?;
                closed_in.push((src, kind, *edge_id));
            }
            if let Some(outs) = state.out_edges.get_mut(&src) {
                outs.remove(&(id, kind));
            }
        }
    }
    Ok(())
}

fn update_configs(vcs: &dyn Vcs, state: &mut WalkState, path: &str, blob: &[u8]) {
    if path == "go.mod" {
        if let Ok(content) = vcs.blob(blob) {
            let text = String::from_utf8_lossy(&content);
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("module ") {
                    state.go_module = Some(rest.trim().to_string());
                    break;
                }
            }
        }
    } else if path.ends_with("Cargo.toml") {
        if let Ok(content) = vcs.blob(blob) {
            let text = String::from_utf8_lossy(&content);
            let mut in_package = false;
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    in_package = t == "[package]";
                }
                if in_package {
                    if let Some(rest) = t.strip_prefix("name") {
                        let name = rest
                            .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                            .trim_matches('"')
                            .replace('-', "_");
                        if !name.is_empty() {
                            let dir = crate::vcs::parent_dir(path);
                            let src = if dir.is_empty() {
                                "src".to_string()
                            } else {
                                format!("{dir}/src")
                            };
                            state.rust_crates.insert(name, src);
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// Parse blobs with the cache in front: the same blob oid is parsed once,
/// ever. Cache misses parse in parallel.
fn parse_blobs(
    store: &Store,
    vcs: &dyn Vcs,
    state: &WalkState,
    jobs: Vec<(FileId, String, Vec<u8>)>,
) -> Result<Vec<(FileId, String, ParsedFile)>> {
    let mut done: Vec<(FileId, String, ParsedFile)> = Vec::new();
    let mut misses: Vec<(FileId, String, Lang, Vec<u8>)> = Vec::new();
    for (id, path, oid) in jobs {
        if oid.iter().all(|b| *b == 0) {
            continue;
        }
        let Some(lang) = state.lang_of.get(&id).copied() else {
            continue;
        };
        match store.blob_parse_get(&oid, PARSER_VERSION)? {
            Some(pf) => done.push((id, path, pf)),
            None => misses.push((id, path, lang, oid)),
        }
    }
    type FetchedBlob = (FileId, String, Lang, Vec<u8>, Vec<u8>);
    type ParsedBlob = (FileId, String, Lang, Vec<u8>, ParsedFile);
    let contents: Vec<FetchedBlob> = misses
        .into_iter()
        .filter_map(|(id, path, lang, oid)| vcs.blob(&oid).ok().map(|c| (id, path, lang, oid, c)))
        .collect();
    let parsed: Vec<ParsedBlob> = contents
        .into_par_iter()
        .map(|(id, path, lang, oid, content)| {
            let pf = langs::extract(lang, &content);
            (id, path, lang, oid, pf)
        })
        .collect();
    for (id, path, lang, oid, pf) in parsed {
        store.blob_parse_put(&oid, lang.as_str(), &pf, PARSER_VERSION)?;
        done.push((id, path, pf));
    }
    Ok(done)
}

fn update_symbols(
    store: &Store,
    state: &mut WalkState,
    id: FileId,
    pf: &ParsedFile,
    commit_id: CommitId,
) -> Result<()> {
    let new: BTreeSet<String> = pf.defs.iter().map(|d| d.name.clone()).collect();
    let old = state.defs_by_file.get(&id).cloned().unwrap_or_default();
    for gone in old.difference(&new) {
        store.symbol_close(gone, id, commit_id)?;
        if let Some(set) = state.symbols.get_mut(gone) {
            set.remove(&id);
            if set.is_empty() {
                state.symbols.remove(gone);
            }
        }
    }
    for added in new.difference(&old) {
        let kind = pf
            .defs
            .iter()
            .find(|d| &d.name == added)
            .map(|d| d.kind.as_str())
            .unwrap_or("def");
        store.symbol_open(added, id, kind, commit_id)?;
        state.symbols.entry(added.clone()).or_default().insert(id);
    }
    state.defs_by_file.insert(id, new);
    Ok(())
}

fn apply_edge_diff(
    store: &Store,
    state: &mut WalkState,
    src: FileId,
    resolved: BTreeSet<(FileId, EdgeKind, pal_core::Resolution)>,
    commit_id: CommitId,
) -> Result<()> {
    let new: BTreeSet<(FileId, EdgeKind)> = resolved.iter().map(|(d, k, _)| (*d, *k)).collect();
    let old = state.out_edges.get(&src).cloned().unwrap_or_default();
    for (dst, kind) in old.difference(&new) {
        if let Some(edge_id) = state.edge_ids.get(&(src, *dst, *kind)) {
            store.interval_close(*edge_id, commit_id)?;
        }
        if let Some(ins) = state.in_edges.get_mut(dst) {
            ins.remove(&(src, *kind));
        }
    }
    for (dst, kind, res) in &resolved {
        if !old.contains(&(*dst, *kind)) {
            let edge_id = store.edge_get_or_create(src, *dst, *kind, *res)?;
            store.interval_open(edge_id, commit_id)?;
            state.edge_ids.insert((src, *dst, *kind), edge_id);
            state.in_edges.entry(*dst).or_default().insert((src, *kind));
        }
    }
    state.out_edges.insert(src, new);
    Ok(())
}

/// Seed live state from a full tree (for indexes that start mid-history).
fn seed_from_tree(
    store: &Store,
    vcs: &dyn Vcs,
    state: &mut WalkState,
    parent_oid: &[u8],
    _params: &Params,
) -> Result<()> {
    let files = vcs.tree_files(parent_oid)?;
    let seed_commit = CommitId(0);
    let mut jobs: Vec<(FileId, String, Vec<u8>)> = Vec::new();
    for (path, blob) in files {
        if is_excluded_path(&path) {
            continue;
        }
        let lang = Lang::from_path(&path);
        let id = store.insert_file(
            &path,
            lang.map(|l| l.as_str()),
            seed_commit,
            lang.map(|l| l.is_doc()).unwrap_or(false),
        )?;
        state.live.insert(path.clone(), id);
        if let Some(l) = lang {
            state.lang_of.insert(id, l);
            jobs.push((id, path.clone(), blob.clone()));
        }
        update_configs(vcs, state, &path, &blob);
    }
    // Parse and resolve the seeded tree so the pre-history edge set exists.
    let parsed = parse_blobs(store, vcs, state, jobs)?;
    for (id, _, pf) in &parsed {
        update_symbols(store, state, *id, pf, seed_commit)?;
    }
    for (id, path, pf) in &parsed {
        let Some(lang) = state.lang_of.get(id).copied() else {
            continue;
        };
        let ctx = ResolveCtx {
            live: &state.live,
            symbols: &state.symbols,
            go_module: state.go_module.as_deref(),
            rust_crates: &state.rust_crates,
        };
        let resolved = resolve_file(&ctx, *id, path, lang, pf);
        state.imports_total += resolved.imports_total;
        state.imports_resolved += resolved.imports_resolved;
        apply_edge_diff(store, state, *id, resolved.edges, seed_commit)?;
    }
    Ok(())
}

/// Rebuild walk state from the database for incremental indexing.
fn load_state(store: &Store, vcs: &dyn Vcs) -> Result<WalkState> {
    let mut state = WalkState::default();
    let files = store.all_files()?;
    for f in &files {
        if let Some(p) = &f.current_path {
            state.live.insert(p.clone(), f.id);
            if let Some(l) = f.lang.as_deref().and_then(Lang::parse_str) {
                state.lang_of.insert(f.id, l);
            }
        }
    }
    // Dead files eligible for resurrection.
    for f in &files {
        if f.current_path.is_some() || f.died_commit.is_none() {
            continue;
        }
        let died = f.died_commit.unwrap();
        let died_time = store
            .commit_by_id(died)?
            .map(|c| c.author_time)
            .unwrap_or(0);
        let path = store.display_path(f.id)?;
        let last_blob: Option<Vec<u8>> = store
            .conn
            .query_row(
                "SELECT blob_oid FROM touches WHERE file_id=?1 AND blob_oid IS NOT NULL
                 ORDER BY commit_id DESC LIMIT 1",
                [f.id.0],
                |r| r.get(0),
            )
            .ok();
        let mut closed_in = Vec::new();
        let mut stmt = store.conn.prepare(
            "SELECT e.src_file, e.kind, e.id FROM edges e
             JOIN edge_intervals i ON i.edge_id = e.id
             WHERE e.dst_file=?1 AND i.died_commit=?2",
        )?;
        let rows = stmt.query_map([f.id.0, died.0], |r| {
            Ok((FileId(r.get(0)?), r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (src, kind, edge_id) = row?;
            if let Some(k) = EdgeKind::from_i64(kind) {
                closed_in.push((src, k, edge_id));
            }
        }
        state.dead.insert(
            path,
            DeadEntry {
                file: f.id,
                died_time,
                last_blob,
                closed_in,
            },
        );
    }
    // Alive symbols.
    {
        let mut stmt = store
            .conn
            .prepare("SELECT name, file_id FROM symbols WHERE last_commit IS NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, FileId(r.get(1)?))))?;
        for row in rows {
            let (name, id) = row?;
            state.symbols.entry(name.clone()).or_default().insert(id);
            state.defs_by_file.entry(id).or_default().insert(name);
        }
    }
    // Open edges.
    for e in store.all_edges()? {
        let open: bool = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edge_intervals WHERE edge_id=?1 AND died_commit IS NULL",
                [e.id],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        state.edge_ids.insert((e.src, e.dst, e.kind), e.id);
        if open {
            state
                .out_edges
                .entry(e.src)
                .or_default()
                .insert((e.dst, e.kind));
            state
                .in_edges
                .entry(e.dst)
                .or_default()
                .insert((e.src, e.kind));
        }
    }
    // Configs from the current head tree.
    if let Ok(head) = vcs.head_oid() {
        if let Ok(files) = vcs.tree_files(&head) {
            for (path, blob) in files {
                if path == "go.mod" || path.ends_with("Cargo.toml") {
                    update_configs(vcs, &mut state, &path, &blob);
                }
            }
        }
    }
    state.imports_total = meta_u64(store, "imports_total");
    state.imports_resolved = meta_u64(store, "imports_resolved");
    Ok(state)
}
