//! Evolutionary coupling: weighted, decayed co-change accumulation over the
//! non-excluded commit set. See plan section 4.1. Lift, not raw counts.

use anyhow::Result;
use pal_core::metrics::{self, PairAcc, Params};
use pal_core::{CommitId, FileId};
use pal_store::{CochangeRow, Store};
use std::collections::HashMap;

pub struct CochangeReport {
    pub pairs_written: usize,
    pub commits_used: usize,
}

/// Pairs are persisted when raw co-occurrence n >= 2. The reporting gates
/// (n and lift) apply at query time so thresholds can move without a
/// reindex.
pub fn compute(store: &Store, params: &Params) -> Result<CochangeReport> {
    let commits = store.all_commits()?;
    let now = commits.iter().map(|c| c.author_time).max().unwrap_or(0);

    // commit id -> (weight, decayed weight)
    let mut weight_of: HashMap<i64, (f64, f64)> = HashMap::new();
    let mut commits_used = 0usize;
    let mut w_total = 0.0f64;
    for c in &commits {
        if c.weight <= 0.0 {
            continue;
        }
        let age_days = (now - c.author_time) as f64 / metrics::SECONDS_PER_DAY;
        let d = metrics::decay(age_days, params.half_life_days);
        weight_of.insert(c.id.0, (c.weight, c.weight * d));
        w_total += c.weight * d;
        commits_used += 1;
    }

    // Group touches by commit (all_touches is ordered by commit).
    let touches = store.all_touches()?;
    let mut per_commit: Vec<(CommitId, Vec<FileId>)> = Vec::new();
    for (commit, file, _import_only) in touches {
        if !weight_of.contains_key(&commit.0) {
            continue;
        }
        match per_commit.last_mut() {
            Some((c, files)) if *c == commit => files.push(file),
            _ => per_commit.push((commit, vec![file])),
        }
    }

    let mut file_support: HashMap<FileId, f64> = HashMap::new();
    let mut pairs: HashMap<(FileId, FileId), PairAcc> = HashMap::new();
    for (commit, files) in &per_commit {
        let (w, wd) = weight_of[&commit.0];
        for f in files {
            *file_support.entry(*f).or_default() += wd;
        }
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let (a, b) = if files[i].0 <= files[j].0 {
                    (files[i], files[j])
                } else {
                    (files[j], files[i])
                };
                if a == b {
                    continue;
                }
                pairs.entry((a, b)).or_default().add(commit.0, w, wd);
            }
        }
    }

    store.cochange_clear()?;
    store.begin()?;
    let mut written = 0usize;
    for ((a, b), acc) in pairs {
        if acc.n < 2 {
            continue;
        }
        let w_a = file_support.get(&a).copied().unwrap_or(0.0);
        let w_b = file_support.get(&b).copied().unwrap_or(0.0);
        let row = CochangeRow {
            a,
            b,
            n: acc.n,
            w_support: acc.w_support,
            w_decayed: acc.w_decayed,
            conf_ab: metrics::confidence(acc.w_decayed, w_a),
            conf_ba: metrics::confidence(acc.w_decayed, w_b),
            lift: metrics::lift(acc.w_decayed, w_a, w_b, w_total),
            first_commit: CommitId(acc.first_commit),
            last_commit: CommitId(acc.last_commit),
        };
        store.cochange_insert(&row)?;
        written += 1;
    }
    store.commit_tx()?;
    Ok(CochangeReport {
        pairs_written: written,
        commits_used,
    })
}
