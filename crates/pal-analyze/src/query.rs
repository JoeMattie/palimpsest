//! Read-side queries: everything the CLI serves. Every result row carries
//! provenance, never a bare score.

use anyhow::Result;
use pal_core::metrics::{self, Params};
use pal_core::time::{date_str, quarter_str};
use pal_core::{Direction, Evidence, FileId, Relation};
use pal_store::{short_oid, CommitRow, EdgeRow, Store};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("file not found in history: {0}")]
    FileNotFound(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitRef {
    pub oid: String,
    pub date: String,
    pub subject: String,
}

pub struct Ctx {
    pub params: Params,
    commits: HashMap<i64, CommitRow>,
    now: i64,
    earliest: i64,
}

impl Ctx {
    pub fn load(store: &Store, params: Params) -> Result<Self> {
        let commits: HashMap<i64, CommitRow> = store
            .all_commits()?
            .into_iter()
            .map(|c| (c.id.0, c))
            .collect();
        let now = commits.values().map(|c| c.author_time).max().unwrap_or(0);
        let earliest = commits.values().map(|c| c.author_time).min().unwrap_or(0);
        Ok(Ctx {
            params,
            commits,
            now,
            earliest,
        })
    }

    fn commit_ref(&self, id: i64) -> CommitRef {
        if id == 0 {
            return CommitRef {
                oid: "(index-start)".into(),
                date: date_str(self.earliest),
                subject: "before indexed history".into(),
            };
        }
        match self.commits.get(&id) {
            Some(c) => CommitRef {
                oid: short_oid(&c.oid),
                date: date_str(c.author_time),
                subject: c.subject.clone().unwrap_or_default(),
            },
            None => CommitRef {
                oid: format!("#{id}"),
                date: String::new(),
                subject: String::new(),
            },
        }
    }

    fn time_of(&self, id: i64) -> i64 {
        if id == 0 {
            self.earliest
        } else {
            self.commits
                .get(&id)
                .map(|c| c.author_time)
                .unwrap_or(self.now)
        }
    }
}

pub fn resolve_file(store: &Store, path: &str) -> Result<pal_store::FileRow, QueryError> {
    let clean = path.trim_start_matches("./");
    store
        .file_by_path(clean)
        .map_err(|e| QueryError::Other(e.into()))?
        .ok_or_else(|| QueryError::FileNotFound(path.to_string()))
}

// ---- graph loading ----

struct EdgeInfo {
    row: EdgeRow,
    alive: bool,
    last_died: Option<i64>,
    total_lifetime_days: i64,
    intervals: Vec<(i64, Option<i64>)>,
}

struct Graph {
    edges: Vec<EdgeInfo>,
    by_src: HashMap<FileId, Vec<usize>>,
    by_dst: HashMap<FileId, Vec<usize>>,
}

fn load_graph(store: &Store, ctx: &Ctx) -> Result<Graph> {
    let rows = store.all_edges()?;
    let mut intervals: HashMap<i64, Vec<(i64, Option<i64>)>> = HashMap::new();
    for iv in store.all_intervals()? {
        intervals
            .entry(iv.edge_id)
            .or_default()
            .push((iv.born.0, iv.died.map(|d| d.0)));
    }
    let mut edges = Vec::with_capacity(rows.len());
    let mut by_src: HashMap<FileId, Vec<usize>> = HashMap::new();
    let mut by_dst: HashMap<FileId, Vec<usize>> = HashMap::new();
    for row in rows {
        let ivs = intervals.remove(&row.id).unwrap_or_default();
        let alive = ivs.iter().any(|(_, d)| d.is_none());
        let last_died = ivs.last().and_then(|(_, d)| *d);
        let mut lifetime = 0i64;
        for (b, d) in &ivs {
            let bt = ctx.time_of(*b);
            let dt = d.map(|d| ctx.time_of(d)).unwrap_or(ctx.now);
            lifetime += ((dt - bt) as f64 / metrics::SECONDS_PER_DAY) as i64;
        }
        let ix = edges.len();
        by_src.entry(row.src).or_default().push(ix);
        by_dst.entry(row.dst).or_default().push(ix);
        edges.push(EdgeInfo {
            row,
            alive,

            last_died,
            total_lifetime_days: lifetime,
            intervals: ivs,
        });
    }
    Ok(Graph {
        edges,
        by_src,
        by_dst,
    })
}

impl Graph {
    fn neighbors(&self, f: FileId) -> impl Iterator<Item = usize> + '_ {
        self.by_src
            .get(&f)
            .into_iter()
            .flatten()
            .chain(self.by_dst.get(&f).into_iter().flatten())
            .copied()
    }
}

/// Undirected BFS over a filtered edge set; returns distance and parent maps.
fn bfs(
    graph: &Graph,
    start: FileId,
    max_depth: u8,
    keep: impl Fn(&EdgeInfo) -> bool,
) -> (HashMap<FileId, u8>, HashMap<FileId, FileId>) {
    let mut dist: HashMap<FileId, u8> = HashMap::new();
    let mut parent: HashMap<FileId, FileId> = HashMap::new();
    let mut q = VecDeque::new();
    dist.insert(start, 0);
    q.push_back(start);
    while let Some(cur) = q.pop_front() {
        let d = dist[&cur];
        if d >= max_depth {
            continue;
        }
        for ix in graph.neighbors(cur) {
            let e = &graph.edges[ix];
            if !keep(e) {
                continue;
            }
            let other = if e.row.src == cur {
                e.row.dst
            } else {
                e.row.src
            };
            if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(other) {
                e.insert(d + 1);
                parent.insert(other, cur);
                q.push_back(other);
            }
        }
    }
    (dist, parent)
}

// ---- blast ----

#[derive(Debug, Clone, Copy)]
pub struct KindFilter {
    pub live: bool,
    pub ghost: bool,
    pub cochange: bool,
    pub doc: bool,
    pub transitive: bool,
}

impl Default for KindFilter {
    fn default() -> Self {
        KindFilter {
            live: true,
            ghost: true,
            cochange: true,
            doc: true,
            transitive: true,
        }
    }
}

impl KindFilter {
    pub fn parse(s: &str) -> Self {
        let mut f = KindFilter {
            live: false,
            ghost: false,
            cochange: false,
            doc: false,
            transitive: false,
        };
        for part in s.split(',') {
            match part.trim() {
                "live" => f.live = true,
                "ghost" => f.ghost = true,
                "cochange" => f.cochange = true,
                "doc" => f.doc = true,
                "transitive" => f.transitive = true,
                _ => {}
            }
        }
        f
    }
}

#[derive(Serialize)]
pub struct QueryInfo {
    pub file: String,
    pub file_id: i64,
    pub head: String,
}

#[derive(Serialize)]
pub struct BlastOutput {
    pub schema: i64,
    pub query: QueryInfo,
    pub results: Vec<Relation>,
    pub truncated: bool,
    pub caveats: Vec<String>,
}

pub struct BlastOptions {
    pub depth: u8,
    pub limit: usize,
    pub min_confidence: f64,
    pub kinds: KindFilter,
}

impl Default for BlastOptions {
    fn default() -> Self {
        BlastOptions {
            depth: 2,
            limit: 20,
            min_confidence: 0.2,
            kinds: KindFilter::default(),
        }
    }
}

pub fn blast(
    store: &Store,
    ctx: &Ctx,
    path: &str,
    opts: &BlastOptions,
) -> Result<BlastOutput, QueryError> {
    let file = resolve_file(store, path)?;
    let q = file.id;
    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;
    let mut evidence: HashMap<FileId, Vec<Evidence>> = HashMap::new();
    let mut caveats = Vec::new();
    if file.current_path.is_none() {
        caveats.push(format!(
            "{} is deleted at HEAD; results describe its historical neighborhood",
            store.display_path(q).unwrap_or_default()
        ));
    }

    // Ghost lookups by edge id.
    let ghost_rows: HashMap<i64, pal_store::GhostRow> = store
        .all_ghosts()
        .map_err(|e| QueryError::Other(e.into()))?
        .into_iter()
        .map(|g| (g.edge_id, g))
        .collect();

    // Live structural edges and ghosts, both directions.
    for ix in graph.neighbors(q) {
        let e = &graph.edges[ix];
        let (other, direction) = if e.row.src == q {
            (e.row.dst, Direction::Out)
        } else {
            (e.row.src, Direction::In)
        };
        if e.alive && opts.kinds.live {
            evidence
                .entry(other)
                .or_default()
                .push(Evidence::Structural {
                    kind: e.row.kind,
                    alive: true,
                    resolution: e.row.resolution,
                    direction,
                });
        }
        if opts.kinds.ghost {
            if let Some(g) = ghost_rows.get(&e.row.id) {
                let sever = ctx.commit_ref(g.severed_commit.0);
                evidence.entry(other).or_default().push(Evidence::Ghost {
                    kind: e.row.kind,
                    severed_at: sever.oid.clone(),
                    severed_date: sever.date.clone(),
                    lifetime_days: g.lifetime_days,
                    cochanges_since: g.cochanges_since,
                    confidence_since: g.conf_since,
                    severing_subject: sever.subject.clone(),
                    direction,
                });
            }
        }
    }

    // Co-change partners.
    if opts.kinds.cochange {
        for row in store
            .cochange_for_file(q)
            .map_err(|e| QueryError::Other(e.into()))?
        {
            let other = if row.a == q { row.b } else { row.a };
            let conf = if row.a == q { row.conf_ab } else { row.conf_ba };
            let strong = row.n >= ctx.params.min_cochange_n && row.lift >= ctx.params.min_lift;
            let already = evidence.contains_key(&other);
            if !strong && !already {
                continue;
            }
            // Skip dead partners for standalone co-change rows.
            if !already {
                let is_alive = store
                    .file_by_id(other)
                    .ok()
                    .flatten()
                    .map(|f| f.current_path.is_some())
                    .unwrap_or(false);
                if !is_alive {
                    continue;
                }
            }
            evidence.entry(other).or_default().push(Evidence::Cochange {
                n: row.n,
                support: row.w_decayed,
                confidence: conf,
                lift: row.lift,
                last: ctx.commit_ref(row.last_commit.0).date,
            });
        }
    }

    // Docs that reference this file, with drift.
    if opts.kinds.doc {
        for ix in graph.by_dst.get(&q).into_iter().flatten() {
            let e = &graph.edges[*ix];
            if e.row.kind != pal_core::EdgeKind::DocRef || !e.alive {
                continue;
            }
            let doc = e.row.src;
            let behind = commits_behind(store, ctx, doc, q).unwrap_or(0);
            if behind > 0 {
                let doc_path = store.display_path(doc).unwrap_or_default();
                evidence.entry(doc).or_default().push(Evidence::DocDrift {
                    doc: doc_path,
                    commits_behind: behind,
                });
            }
        }
    }

    // Transitive: near in the union graph, far or unreachable live.
    if opts.kinds.transitive && opts.depth >= 2 {
        let min_life = ctx.params.ghost_min_lifetime_days;
        let union_keep = |e: &EdgeInfo| e.alive || e.total_lifetime_days >= min_life;
        let (union_dist, union_parent) = bfs(&graph, q, opts.depth, union_keep);
        let (live_dist, _) = bfs(&graph, q, 3, |e| e.alive);
        for (node, d) in &union_dist {
            if *node == q || *d == 0 || evidence.contains_key(node) {
                continue;
            }
            let head_dist = live_dist.get(node).copied();
            if head_dist.is_some_and(|hd| hd <= *d) {
                continue;
            }
            let node_alive = store
                .file_by_id(*node)
                .ok()
                .flatten()
                .map(|f| f.current_path.is_some())
                .unwrap_or(false);
            if !node_alive {
                continue;
            }
            let mut via = Vec::new();
            let mut cur = *node;
            while let Some(p) = union_parent.get(&cur) {
                if *p != q {
                    via.push(store.display_path(*p).unwrap_or_default());
                }
                cur = *p;
                if cur == q {
                    break;
                }
            }
            via.reverse();
            evidence
                .entry(*node)
                .or_default()
                .push(Evidence::Transitive {
                    via,
                    union_dist: *d,
                    head_dist,
                });
        }
    }

    let mut results: Vec<Relation> = evidence
        .into_iter()
        .filter_map(|(id, ev)| {
            let rank = metrics::blend_rank(&ev);
            if rank < opts.min_confidence {
                return None;
            }
            let path = store.display_path(id).ok()?;
            Some(Relation {
                path,
                file_id: id,
                rank: (rank * 100.0).round() / 100.0,
                evidence: ev,
            })
        })
        .collect();
    results.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let truncated = results.len() > opts.limit;
    results.truncate(opts.limit);

    let head = store
        .meta_get("head_oid")
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(BlastOutput {
        schema: SCHEMA_VERSION,
        query: QueryInfo {
            file: store.display_path(q).unwrap_or_else(|_| path.to_string()),
            file_id: q.0,
            head: head.chars().take(12).collect(),
        },
        results,
        truncated,
        caveats,
    })
}

/// Non-excluded commits touching `code` since the last commit touching `doc`.
fn commits_behind(store: &Store, ctx: &Ctx, doc: FileId, code: FileId) -> Result<i64> {
    let doc_last = store
        .touches_for_file(doc)?
        .last()
        .map(|(c, _)| c.0)
        .unwrap_or(0);
    let n = store
        .touches_for_file(code)?
        .iter()
        .filter(|(c, _)| {
            c.0 > doc_last
                && ctx
                    .commits
                    .get(&c.0)
                    .map(|cm| cm.weight > 0.0)
                    .unwrap_or(false)
        })
        .count();
    Ok(n as i64)
}

// ---- ghosts listing ----

#[derive(Serialize)]
pub struct GhostEntry {
    pub from: String,
    pub to: String,
    pub kind: &'static str,
    pub severed_at: CommitRef,
    pub lifetime_days: i64,
    pub cochanges_since: i64,
    pub confidence_since: f64,
    pub score: f64,
}

#[derive(Serialize)]
pub struct GhostsOutput {
    pub schema: i64,
    pub results: Vec<GhostEntry>,
    pub truncated: bool,
}

pub fn ghosts_list(
    store: &Store,
    ctx: &Ctx,
    file: Option<&str>,
    limit: usize,
) -> Result<GhostsOutput, QueryError> {
    let filter_id = match file {
        Some(p) => Some(resolve_file(store, p)?.id),
        None => None,
    };
    let mut results = Vec::new();
    for g in store
        .all_ghosts()
        .map_err(|e| QueryError::Other(e.into()))?
    {
        let Some(edge) = store
            .edge_by_id(g.edge_id)
            .map_err(|e| QueryError::Other(e.into()))?
        else {
            continue;
        };
        if let Some(f) = filter_id {
            if edge.src != f && edge.dst != f {
                continue;
            }
        }
        results.push(GhostEntry {
            from: store.display_path(edge.src).unwrap_or_default(),
            to: store.display_path(edge.dst).unwrap_or_default(),
            kind: edge.kind.as_str(),
            severed_at: ctx.commit_ref(g.severed_commit.0),
            lifetime_days: g.lifetime_days,
            cochanges_since: g.cochanges_since,
            confidence_since: (g.conf_since * 100.0).round() / 100.0,
            score: (g.score * 100.0).round() / 100.0,
        });
    }
    let truncated = results.len() > limit;
    results.truncate(limit);
    Ok(GhostsOutput {
        schema: SCHEMA_VERSION,
        results,
        truncated,
    })
}

// ---- cochange listing ----

#[derive(Serialize)]
pub struct CochangeEntry {
    pub path: String,
    pub n: i64,
    pub confidence: f64,
    pub lift: f64,
    pub first: String,
    pub last: String,
    pub structural_edge: bool,
}

#[derive(Serialize)]
pub struct CochangeOutput {
    pub schema: i64,
    pub query: QueryInfo,
    pub results: Vec<CochangeEntry>,
    pub truncated: bool,
}

/// Pure evolutionary coupling for one file: pairs above the reporting gates,
/// flagged when a structural edge also exists (live or dead).
pub fn cochange_list(
    store: &Store,
    ctx: &Ctx,
    path: &str,
    limit: usize,
    include_structural: bool,
) -> Result<CochangeOutput, QueryError> {
    let file = resolve_file(store, path)?;
    let q = file.id;
    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;
    let structural: HashSet<FileId> = graph
        .neighbors(q)
        .map(|ix| {
            let e = &graph.edges[ix];
            if e.row.src == q {
                e.row.dst
            } else {
                e.row.src
            }
        })
        .collect();
    let mut results = Vec::new();
    for row in store
        .cochange_for_file(q)
        .map_err(|e| QueryError::Other(e.into()))?
    {
        if row.n < ctx.params.min_cochange_n || row.lift < ctx.params.min_lift {
            continue;
        }
        let other = if row.a == q { row.b } else { row.a };
        let has_edge = structural.contains(&other);
        if has_edge && !include_structural {
            continue;
        }
        let alive = store
            .file_by_id(other)
            .ok()
            .flatten()
            .map(|f| f.current_path.is_some())
            .unwrap_or(false);
        if !alive {
            continue;
        }
        let conf = if row.a == q { row.conf_ab } else { row.conf_ba };
        results.push(CochangeEntry {
            path: store.display_path(other).unwrap_or_default(),
            n: row.n,
            confidence: (conf * 100.0).round() / 100.0,
            lift: (row.lift * 10.0).round() / 10.0,
            first: ctx.commit_ref(row.first_commit.0).date,
            last: ctx.commit_ref(row.last_commit.0).date,
            structural_edge: has_edge,
        });
    }
    let truncated = results.len() > limit;
    results.truncate(limit);
    let head = store
        .meta_get("head_oid")
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(CochangeOutput {
        schema: SCHEMA_VERSION,
        query: QueryInfo {
            file: store.display_path(q).unwrap_or_default(),
            file_id: q.0,
            head: head.chars().take(12).collect(),
        },
        results,
        truncated,
    })
}

// ---- why ----

#[derive(Serialize)]
pub struct WhyInterval {
    pub born: CommitRef,
    pub died: Option<CommitRef>,
}

#[derive(Serialize)]
pub struct WhyEdge {
    pub from: String,
    pub to: String,
    pub kind: &'static str,
    pub resolution: &'static str,
    pub alive: bool,
    pub intervals: Vec<WhyInterval>,
    pub ghost: Option<WhyGhost>,
}

#[derive(Serialize)]
pub struct WhyGhost {
    pub severed_at: CommitRef,
    pub severing_author: String,
    pub severing_body: String,
    pub lifetime_days: i64,
    pub cochanges_since: i64,
    pub confidence_since: f64,
    pub score: f64,
}

#[derive(Serialize)]
pub struct WhyOutput {
    pub schema: i64,
    pub a: String,
    pub b: String,
    pub edges: Vec<WhyEdge>,
    pub cochange: Option<CochangeEntry>,
    pub caveats: Vec<String>,
}

pub fn why(store: &Store, ctx: &Ctx, a: &str, b: &str) -> Result<WhyOutput, QueryError> {
    let fa = resolve_file(store, a)?;
    let fb = resolve_file(store, b)?;
    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;
    let ghost_rows: HashMap<i64, pal_store::GhostRow> = store
        .all_ghosts()
        .map_err(|e| QueryError::Other(e.into()))?
        .into_iter()
        .map(|g| (g.edge_id, g))
        .collect();
    let mut edges = Vec::new();
    for e in &graph.edges {
        let between = (e.row.src == fa.id && e.row.dst == fb.id)
            || (e.row.src == fb.id && e.row.dst == fa.id);
        if !between {
            continue;
        }
        let ghost = ghost_rows.get(&e.row.id).map(|g| {
            let full = ctx.commits.get(&g.severed_commit.0);
            WhyGhost {
                severed_at: ctx.commit_ref(g.severed_commit.0),
                severing_author: full.and_then(|c| c.author.clone()).unwrap_or_default(),
                severing_body: store
                    .conn
                    .query_row(
                        "SELECT body FROM commits WHERE id=?1",
                        [g.severed_commit.0],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                lifetime_days: g.lifetime_days,
                cochanges_since: g.cochanges_since,
                confidence_since: (g.conf_since * 100.0).round() / 100.0,
                score: (g.score * 100.0).round() / 100.0,
            }
        });
        edges.push(WhyEdge {
            from: store.display_path(e.row.src).unwrap_or_default(),
            to: store.display_path(e.row.dst).unwrap_or_default(),
            kind: e.row.kind.as_str(),
            resolution: e.row.resolution.as_str(),
            alive: e.alive,
            intervals: e
                .intervals
                .iter()
                .map(|(born, died)| WhyInterval {
                    born: ctx.commit_ref(*born),
                    died: died.map(|d| ctx.commit_ref(d)),
                })
                .collect(),
            ghost,
        });
    }
    let cochange = store
        .cochange_pair(fa.id, fb.id)
        .map_err(|e| QueryError::Other(e.into()))?
        .map(|row| CochangeEntry {
            path: store.display_path(fb.id).unwrap_or_default(),
            n: row.n,
            confidence: ((if row.a == fa.id {
                row.conf_ab
            } else {
                row.conf_ba
            }) * 100.0)
                .round()
                / 100.0,
            lift: (row.lift * 10.0).round() / 10.0,
            first: ctx.commit_ref(row.first_commit.0).date,
            last: ctx.commit_ref(row.last_commit.0).date,
            structural_edge: !edges.is_empty(),
        });
    let mut caveats = Vec::new();
    if edges.is_empty() && cochange.is_none() {
        caveats.push("no recorded relationship between these files".to_string());
    }
    Ok(WhyOutput {
        schema: SCHEMA_VERSION,
        a: store.display_path(fa.id).unwrap_or_default(),
        b: store.display_path(fb.id).unwrap_or_default(),
        edges,
        cochange,
        caveats,
    })
}

// ---- path ----

#[derive(Serialize)]
pub struct Hop {
    pub from: String,
    pub to: String,
    pub kind: &'static str,
    pub alive: bool,
    pub died: Option<CommitRef>,
}

#[derive(Serialize)]
pub struct PathOutput {
    pub schema: i64,
    pub a: String,
    pub b: String,
    pub live_path: Option<Vec<Hop>>,
    pub union_path: Option<Vec<Hop>>,
    pub broke_at: Option<CommitRef>,
}

pub fn path_query(
    store: &Store,
    ctx: &Ctx,
    a: &str,
    b: &str,
    as_of: Option<&str>,
) -> Result<PathOutput, QueryError> {
    let fa = resolve_file(store, a)?;
    let fb = resolve_file(store, b)?;
    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;

    let cutoff: Option<i64> = match as_of {
        Some(rev) => Some(resolve_as_of(ctx, rev)?),
        None => None,
    };
    let alive_at = |e: &EdgeInfo| match cutoff {
        None => e.alive,
        Some(c) => e
            .intervals
            .iter()
            .any(|(born, died)| *born <= c && died.map(|d| d > c).unwrap_or(true)),
    };

    let reconstruct =
        |parent: &HashMap<FileId, FileId>, keep: &dyn Fn(&EdgeInfo) -> bool| -> Option<Vec<Hop>> {
            let mut hops = Vec::new();
            let mut cur = fb.id;
            while cur != fa.id {
                let prev = *parent.get(&cur)?;
                // Find the edge between prev and cur that the filter kept.
                let ix = graph.neighbors(prev).find(|ix| {
                    let e = &graph.edges[*ix];
                    keep(e)
                        && ((e.row.src == prev && e.row.dst == cur)
                            || (e.row.src == cur && e.row.dst == prev))
                })?;
                let e = &graph.edges[ix];
                hops.push(Hop {
                    from: store.display_path(e.row.src).unwrap_or_default(),
                    to: store.display_path(e.row.dst).unwrap_or_default(),
                    kind: e.row.kind.as_str(),
                    alive: e.alive,
                    died: e.last_died.map(|d| ctx.commit_ref(d)),
                });
                cur = prev;
            }
            hops.reverse();
            Some(hops)
        };

    let (live_dist, live_parent) = bfs(&graph, fa.id, 6, alive_at);
    let live_path = if live_dist.contains_key(&fb.id) {
        reconstruct(&live_parent, &|e| alive_at(e))
    } else {
        None
    };

    let min_life = ctx.params.ghost_min_lifetime_days;
    let union_keep = |e: &EdgeInfo| e.alive || e.total_lifetime_days >= min_life;
    let (union_dist, union_parent) = bfs(&graph, fa.id, 6, union_keep);
    let union_path = if live_path.is_none() && union_dist.contains_key(&fb.id) {
        reconstruct(&union_parent, &union_keep)
    } else {
        None
    };
    let broke_at = union_path.as_ref().and_then(|hops| {
        hops.iter()
            .filter(|h| !h.alive)
            .filter_map(|h| h.died.clone())
            .max_by(|x, y| x.date.cmp(&y.date))
    });
    Ok(PathOutput {
        schema: SCHEMA_VERSION,
        a: store.display_path(fa.id).unwrap_or_default(),
        b: store.display_path(fb.id).unwrap_or_default(),
        live_path,
        union_path,
        broke_at,
    })
}

fn resolve_as_of(ctx: &Ctx, rev: &str) -> Result<i64, QueryError> {
    // Date form: newest commit at or before that date.
    if rev.len() == 10 && rev.as_bytes().get(4) == Some(&b'-') {
        let best = ctx
            .commits
            .values()
            .filter(|c| date_str(c.author_time).as_str() <= rev)
            .max_by_key(|c| c.id.0)
            .map(|c| c.id.0);
        return best
            .ok_or_else(|| QueryError::Other(anyhow::anyhow!("no commits at or before {rev}")));
    }
    // Oid prefix form.
    let lower = rev.to_ascii_lowercase();
    let matched: Vec<i64> = ctx
        .commits
        .values()
        .filter(|c| pal_store::full_oid(&c.oid).starts_with(&lower))
        .map(|c| c.id.0)
        .collect();
    match matched.as_slice() {
        [one] => Ok(*one),
        [] => Err(QueryError::Other(anyhow::anyhow!(
            "rev {rev} not found in indexed history"
        ))),
        _ => Err(QueryError::Other(anyhow::anyhow!("rev {rev} is ambiguous"))),
    }
}

// ---- timeline ----

#[derive(Serialize)]
pub struct TimelinePathSpan {
    pub path: String,
    pub from: CommitRef,
    pub to: Option<CommitRef>,
    pub confidence: f64,
}

#[derive(Serialize)]
pub struct TimelineEdgeEvent {
    pub commit: CommitRef,
    pub event: &'static str,
    pub kind: &'static str,
    pub other: String,
    pub direction: &'static str,
}

#[derive(Serialize)]
pub struct ChurnBucket {
    pub period: String,
    pub touches: i64,
    pub lines: i64,
}

#[derive(Serialize)]
pub struct TimelineOutput {
    pub schema: i64,
    pub file: String,
    pub born: CommitRef,
    pub died: Option<CommitRef>,
    pub paths: Vec<TimelinePathSpan>,
    pub edge_events: Vec<TimelineEdgeEvent>,
    pub churn: Vec<ChurnBucket>,
}

pub fn timeline(store: &Store, ctx: &Ctx, path: &str) -> Result<TimelineOutput, QueryError> {
    let file = resolve_file(store, path)?;
    let q = file.id;
    let paths = store
        .file_paths_for(q)
        .map_err(|e| QueryError::Other(e.into()))?
        .into_iter()
        .map(|p| TimelinePathSpan {
            path: p.path,
            from: ctx.commit_ref(p.from_commit.0),
            to: p.to_commit.map(|c| ctx.commit_ref(c.0)),
            confidence: p.confidence,
        })
        .collect();

    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;
    let mut events: Vec<(i64, TimelineEdgeEvent)> = Vec::new();
    for ix in graph.neighbors(q) {
        let e = &graph.edges[ix];
        let (other, dir) = if e.row.src == q {
            (e.row.dst, "out")
        } else {
            (e.row.src, "in")
        };
        let other_path = store.display_path(other).unwrap_or_default();
        for (born, died) in &e.intervals {
            events.push((
                *born,
                TimelineEdgeEvent {
                    commit: ctx.commit_ref(*born),
                    event: "opened",
                    kind: e.row.kind.as_str(),
                    other: other_path.clone(),
                    direction: dir,
                },
            ));
            if let Some(d) = died {
                events.push((
                    *d,
                    TimelineEdgeEvent {
                        commit: ctx.commit_ref(*d),
                        event: "closed",
                        kind: e.row.kind.as_str(),
                        other: other_path.clone(),
                        direction: dir,
                    },
                ));
            }
        }
    }
    events.sort_by_key(|(c, _)| *c);
    let edge_events: Vec<TimelineEdgeEvent> =
        events.into_iter().map(|(_, e)| e).take(200).collect();

    let mut churn: Vec<ChurnBucket> = Vec::new();
    {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT t.commit_id, COALESCE(t.lines_added,0)+COALESCE(t.lines_removed,0)
                 FROM touches t WHERE t.file_id=?1 ORDER BY t.commit_id",
            )
            .map_err(|e| QueryError::Other(e.into()))?;
        let rows = stmt
            .query_map([q.0], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| QueryError::Other(e.into()))?;
        let mut buckets: Vec<(String, i64, i64)> = Vec::new();
        for row in rows {
            let (commit, lines) = row.map_err(|e| QueryError::Other(e.into()))?;
            let quarter = quarter_str(ctx.time_of(commit));
            match buckets.iter_mut().find(|(qq, _, _)| *qq == quarter) {
                Some((_, t, l)) => {
                    *t += 1;
                    *l += lines;
                }
                None => buckets.push((quarter, 1, lines)),
            }
        }
        buckets.sort_by(|a, b| a.0.cmp(&b.0));
        for (period, touches, lines) in buckets {
            churn.push(ChurnBucket {
                period,
                touches,
                lines,
            });
        }
    }

    Ok(TimelineOutput {
        schema: SCHEMA_VERSION,
        file: store.display_path(q).unwrap_or_default(),
        born: ctx.commit_ref(file.born_commit.0),
        died: file.died_commit.map(|c| ctx.commit_ref(c.0)),
        paths,
        edge_events,
        churn,
    })
}

// ---- drift ----

#[derive(Serialize)]
pub struct DriftEntry {
    pub doc: String,
    pub code: String,
    pub commits_behind: i64,
    pub doc_last_touched: String,
    pub code_last_touched: String,
}

#[derive(Serialize)]
pub struct DriftOutput {
    pub schema: i64,
    pub results: Vec<DriftEntry>,
    pub truncated: bool,
}

pub fn drift(store: &Store, ctx: &Ctx, limit: usize) -> Result<DriftOutput, QueryError> {
    let graph = load_graph(store, ctx).map_err(QueryError::Other)?;
    let mut results = Vec::new();
    for e in &graph.edges {
        if e.row.kind != pal_core::EdgeKind::DocRef || !e.alive {
            continue;
        }
        let doc = e.row.src;
        let code = e.row.dst;
        let behind = commits_behind(store, ctx, doc, code).map_err(QueryError::Other)?;
        if behind == 0 {
            continue;
        }
        let doc_last = store
            .touches_for_file(doc)
            .map_err(|e| QueryError::Other(e.into()))?
            .last()
            .map(|(c, _)| ctx.commit_ref(c.0).date)
            .unwrap_or_default();
        let code_last = store
            .touches_for_file(code)
            .map_err(|e| QueryError::Other(e.into()))?
            .last()
            .map(|(c, _)| ctx.commit_ref(c.0).date)
            .unwrap_or_default();
        results.push(DriftEntry {
            doc: store.display_path(doc).unwrap_or_default(),
            code: store.display_path(code).unwrap_or_default(),
            commits_behind: behind,
            doc_last_touched: doc_last,
            code_last_touched: code_last,
        });
    }
    results.sort_by_key(|r| std::cmp::Reverse(r.commits_behind));
    let truncated = results.len() > limit;
    results.truncate(limit);
    Ok(DriftOutput {
        schema: SCHEMA_VERSION,
        results,
        truncated,
    })
}

// ---- hotspots ----

#[derive(Serialize)]
pub struct HotspotEntry {
    pub path: String,
    pub score: f64,
    pub detail: String,
}

#[derive(Serialize)]
pub struct HotspotsOutput {
    pub schema: i64,
    pub by: String,
    pub results: Vec<HotspotEntry>,
    pub truncated: bool,
}

pub fn hotspots(
    store: &Store,
    ctx: &Ctx,
    by: &str,
    limit: usize,
) -> Result<HotspotsOutput, QueryError> {
    let alive: HashMap<FileId, String> = store
        .all_files()
        .map_err(|e| QueryError::Other(e.into()))?
        .into_iter()
        .filter_map(|f| f.current_path.clone().map(|p| (f.id, p)))
        .collect();
    let mut scores: HashMap<FileId, (f64, i64)> = HashMap::new();
    match by {
        "coupling" => {
            for row in store
                .all_cochange()
                .map_err(|e| QueryError::Other(e.into()))?
            {
                if row.lift < ctx.params.min_lift || row.n < ctx.params.min_cochange_n {
                    continue;
                }
                for f in [row.a, row.b] {
                    let e = scores.entry(f).or_default();
                    e.0 += row.w_decayed;
                    e.1 += 1;
                }
            }
        }
        "ghosts" => {
            for g in store
                .all_ghosts()
                .map_err(|e| QueryError::Other(e.into()))?
            {
                if let Some(edge) = store
                    .edge_by_id(g.edge_id)
                    .map_err(|e| QueryError::Other(e.into()))?
                {
                    for f in [edge.src, edge.dst] {
                        let e = scores.entry(f).or_default();
                        e.0 += g.score;
                        e.1 += 1;
                    }
                }
            }
        }
        _ => {
            // churn (default): decayed weighted touch mass.
            for (commit, file, _) in store
                .all_touches()
                .map_err(|e| QueryError::Other(e.into()))?
            {
                let Some(c) = ctx.commits.get(&commit.0) else {
                    continue;
                };
                if c.weight <= 0.0 {
                    continue;
                }
                let age = (ctx.now - c.author_time) as f64 / metrics::SECONDS_PER_DAY;
                let e = scores.entry(file).or_default();
                e.0 += c.weight * metrics::decay(age, ctx.params.half_life_days);
                e.1 += 1;
            }
        }
    }
    let mut results: Vec<HotspotEntry> = scores
        .into_iter()
        .filter_map(|(f, (score, n))| {
            let path = alive.get(&f)?.clone();
            let detail = match by {
                "coupling" => format!("{n} coupled partners"),
                "ghosts" => format!("{n} ghost edges"),
                _ => format!("{n} touches"),
            };
            Some(HotspotEntry {
                path,
                score: (score * 100.0).round() / 100.0,
                detail,
            })
        })
        .collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let truncated = results.len() > limit;
    results.truncate(limit);
    Ok(HotspotsOutput {
        schema: SCHEMA_VERSION,
        by: by.to_string(),
        results,
        truncated,
    })
}

// ---- search ----

#[derive(Serialize)]
pub struct SearchEntry {
    pub commit: CommitRef,
    pub author: String,
}

#[derive(Serialize)]
pub struct SearchOutput {
    pub schema: i64,
    pub results: Vec<SearchEntry>,
}

pub fn search(store: &Store, ctx: &Ctx, query: &str, limit: usize) -> Result<SearchOutput> {
    let results = store
        .fts_search(query, limit as i64)?
        .into_iter()
        .map(|(id, _subject, author)| SearchEntry {
            commit: ctx.commit_ref(id.0),
            author,
        })
        .collect();
    Ok(SearchOutput {
        schema: SCHEMA_VERSION,
        results,
    })
}

// ---- stats ----

#[derive(Serialize)]
pub struct StatsOutput {
    pub schema: i64,
    pub head: String,
    pub indexed_at: Option<String>,
    pub commits: StatsCommits,
    pub files: StatsFiles,
    pub edges: StatsEdges,
    pub cochange_pairs: i64,
    pub imports: StatsImports,
    pub renames: StatsRenames,
}

#[derive(Serialize)]
pub struct StatsCommits {
    pub total: i64,
    pub excluded_too_large: i64,
    pub excluded_mechanical: i64,
    pub merges: i64,
    pub excluded_pct: f64,
}

#[derive(Serialize)]
pub struct StatsFiles {
    pub total: i64,
    pub alive: i64,
    pub dead: i64,
    pub docs: i64,
}

#[derive(Serialize)]
pub struct StatsEdges {
    pub total: i64,
    pub live: i64,
    pub ghosts: i64,
}

#[derive(Serialize)]
pub struct StatsImports {
    pub total: i64,
    pub resolved: i64,
    pub unresolved_ratio: f64,
}

#[derive(Serialize)]
pub struct StatsRenames {
    pub total: i64,
    pub low_confidence: i64,
}

pub fn stats(store: &Store) -> Result<StatsOutput> {
    let total = store.count("SELECT COUNT(*) FROM commits")?;
    let too_large = store.count("SELECT COUNT(*) FROM commits WHERE excluded & 1 != 0")?;
    let mechanical = store.count("SELECT COUNT(*) FROM commits WHERE excluded & 2 != 0")?;
    let merges = store.count("SELECT COUNT(*) FROM commits WHERE is_merge != 0")?;
    let zero_weight = store.count("SELECT COUNT(*) FROM commits WHERE weight <= 0")?;
    let files_total = store.count("SELECT COUNT(*) FROM files")?;
    let files_alive = store.count("SELECT COUNT(*) FROM files WHERE current_path IS NOT NULL")?;
    let docs = store.count("SELECT COUNT(*) FROM files WHERE is_doc != 0")?;
    let edges_total = store.count("SELECT COUNT(*) FROM edges")?;
    let edges_live = store
        .count("SELECT COUNT(DISTINCT edge_id) FROM edge_intervals WHERE died_commit IS NULL")?;
    let ghosts = store.count("SELECT COUNT(*) FROM ghosts")?;
    let cochange_pairs = store.count("SELECT COUNT(*) FROM cochange")?;
    let renames_total =
        store.count("SELECT COUNT(*) FROM file_paths WHERE from_commit != 0")? - files_total;
    let low_conf = store.count("SELECT COUNT(*) FROM file_paths WHERE confidence < 0.8")?;
    let imports_total: i64 = store
        .meta_get("imports_total")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let imports_resolved: i64 = store
        .meta_get("imports_resolved")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Ok(StatsOutput {
        schema: SCHEMA_VERSION,
        head: store.meta_get("head_oid")?.unwrap_or_default(),
        indexed_at: store.meta_get("indexed_at")?,
        commits: StatsCommits {
            total,
            excluded_too_large: too_large,
            excluded_mechanical: mechanical,
            merges,
            excluded_pct: if total > 0 {
                (zero_weight as f64 / total as f64 * 1000.0).round() / 10.0
            } else {
                0.0
            },
        },
        files: StatsFiles {
            total: files_total,
            alive: files_alive,
            dead: files_total - files_alive,
            docs,
        },
        edges: StatsEdges {
            total: edges_total,
            live: edges_live,
            ghosts,
        },
        cochange_pairs,
        imports: StatsImports {
            total: imports_total,
            resolved: imports_resolved,
            unresolved_ratio: if imports_total > 0 {
                ((imports_total - imports_resolved) as f64 / imports_total as f64 * 1000.0).round()
                    / 1000.0
            } else {
                0.0
            },
        },
        renames: StatsRenames {
            total: renames_total.max(0),
            low_confidence: low_conf,
        },
    })
}
