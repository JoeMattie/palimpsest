//! Ghost detection, plan section 4.2: an edge whose latest interval is
//! closed, whose severance was not mechanical, whose endpoints are both
//! alive, and whose endpoints kept co-changing after the severance. The
//! post-severance co-change condition is a gate, not a score term.

use anyhow::Result;
use pal_core::metrics::{self, Params};
use pal_core::{CommitId, FileId};
use pal_store::{GhostRow, Store};
use std::collections::HashMap;

pub struct GhostReport {
    pub candidates: usize,
    pub ghosts: usize,
}

pub fn compute(store: &Store, params: &Params) -> Result<GhostReport> {
    let commits = store.all_commits()?;
    let now = commits.iter().map(|c| c.author_time).max().unwrap_or(0);
    let mut time_of: HashMap<i64, i64> = HashMap::new();
    let mut mechanical: HashMap<i64, bool> = HashMap::new();
    let mut included: HashMap<i64, f64> = HashMap::new();
    let earliest_time = commits.iter().map(|c| c.author_time).min().unwrap_or(0);
    for c in &commits {
        time_of.insert(c.id.0, c.author_time);
        mechanical.insert(c.id.0, c.excluded & pal_core::excluded::MECHANICAL != 0);
        if c.weight > 0.0 {
            let age = (now - c.author_time) as f64 / metrics::SECONDS_PER_DAY;
            included.insert(
                c.id.0,
                c.weight * metrics::decay(age, params.half_life_days),
            );
        }
    }

    // Touch lists per file over included commits, ordered.
    let mut touches_of: HashMap<FileId, Vec<i64>> = HashMap::new();
    for (commit, file, _) in store.all_touches()? {
        if included.contains_key(&commit.0) {
            touches_of.entry(file).or_default().push(commit.0);
        }
    }

    // Latest interval per edge decides candidacy.
    let mut last_interval: HashMap<i64, (CommitId, Option<CommitId>)> = HashMap::new();
    let mut has_open: HashMap<i64, bool> = HashMap::new();
    for iv in store.all_intervals()? {
        if iv.died.is_none() {
            has_open.insert(iv.edge_id, true);
        }
        last_interval.insert(iv.edge_id, (iv.born, iv.died));
    }

    let files: HashMap<FileId, bool> = store
        .all_files()?
        .into_iter()
        .map(|f| (f.id, f.current_path.is_some()))
        .collect();

    store.ghosts_clear()?;
    store.begin()?;
    let mut candidates = 0usize;
    let mut ghosts = 0usize;
    for edge in store.all_edges()? {
        if has_open.get(&edge.id).copied().unwrap_or(false) {
            continue;
        }
        let Some((born, Some(died))) = last_interval.get(&edge.id).copied() else {
            continue;
        };
        candidates += 1;
        // The severance itself must be a real commit, not mechanical churn.
        if mechanical.get(&died.0).copied().unwrap_or(false) {
            continue;
        }
        // Both endpoints alive at HEAD.
        if !files.get(&edge.src).copied().unwrap_or(false)
            || !files.get(&edge.dst).copied().unwrap_or(false)
        {
            continue;
        }
        let born_time = if born.0 == 0 {
            earliest_time
        } else {
            time_of.get(&born.0).copied().unwrap_or(earliest_time)
        };
        let died_time = time_of.get(&died.0).copied().unwrap_or(now);
        let lifetime_days = ((died_time - born_time) as f64 / metrics::SECONDS_PER_DAY) as i64;
        if lifetime_days < params.ghost_min_lifetime_days {
            continue;
        }
        // Post-severance co-change.
        let src_touches = touches_of.get(&edge.src);
        let dst_touches = touches_of.get(&edge.dst);
        let (Some(src_touches), Some(dst_touches)) = (src_touches, dst_touches) else {
            continue;
        };
        let mut w_src = 0.0f64;
        let mut w_both = 0.0f64;
        let mut n_both = 0i64;
        let dst_set: std::collections::HashSet<i64> = dst_touches
            .iter()
            .copied()
            .filter(|c| *c > died.0)
            .collect();
        for c in src_touches.iter().copied().filter(|c| *c > died.0) {
            let wd = included.get(&c).copied().unwrap_or(0.0);
            w_src += wd;
            if dst_set.contains(&c) {
                w_both += wd;
                n_both += 1;
            }
        }
        let conf_since = metrics::confidence(w_both, w_src);
        if n_both < params.ghost_min_cochanges_since || conf_since < params.ghost_min_conf_since {
            continue;
        }
        let severance_age_days = (now - died_time) as f64 / metrics::SECONDS_PER_DAY;
        let score = metrics::ghost_score(
            conf_since,
            n_both,
            severance_age_days,
            lifetime_days,
            params.half_life_days,
        );
        store.ghost_insert(&GhostRow {
            edge_id: edge.id,
            severed_commit: died,
            lifetime_days,
            cochanges_since: n_both,
            conf_since,
            score,
        })?;
        ghosts += 1;
    }
    store.commit_tx()?;
    Ok(GhostReport { candidates, ghosts })
}
