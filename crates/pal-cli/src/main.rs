//! `pal`: the agent surface. No logic lives here, only argument parsing,
//! output serialization, and exit codes.
//!
//! Exit codes: 0 ok, 2 db missing or unreadable, 3 file not found in
//! history, 4 schema version mismatch.

mod human;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pal_analyze::query::{self, BlastOptions, KindFilter, QueryError};
use pal_core::metrics::Params;
use pal_index::vcs::{GitVcs, Vcs};
use pal_index::IndexOptions;
use pal_store::{Store, StoreError};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "pal",
    version,
    about = "palimpsest: how files are actually related, including relationships that no longer exist in the source"
)]
struct Cli {
    /// Path to the index database (default: nearest .pal/index.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Emit machine-readable JSON (stable, versioned schema)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build or extend the index for a repository
    Index {
        /// Repository path (default: current directory)
        path: Option<PathBuf>,
        /// Only index commits after this rev or date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Extend an existing index from its recorded head
        #[arg(long)]
        incremental: bool,
        /// Walk all parents instead of first-parent only
        #[arg(long)]
        all_parents: bool,
        /// Hard-exclude commits touching more files than this
        #[arg(long, default_value_t = 50)]
        max_commit_files: usize,
        /// Recency half-life, e.g. 365d
        #[arg(long, default_value = "365d")]
        half_life: String,
        /// Worker threads for parsing
        #[arg(long)]
        jobs: Option<usize>,
        /// Suppress the progress bar
        #[arg(long)]
        quiet: bool,
    },
    /// Ranked "what else is affected if I change this file"
    Blast {
        file: String,
        #[arg(long, default_value_t = 2)]
        depth: u8,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0.2)]
        min_confidence: f64,
        /// Comma-separated: live,ghost,cochange,doc,transitive
        #[arg(long)]
        kinds: Option<String>,
    },
    /// Ghost edges: severed structural links that still co-change
    Ghosts {
        file: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Evolutionary coupling with no structural edge
    Cochange {
        file: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Also list partners that do have a structural edge
        #[arg(long)]
        include_structural: bool,
    },
    /// Live path, union path, and when it broke
    Path {
        a: String,
        b: String,
        /// Evaluate liveness at a rev or date instead of HEAD
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Full evidence chain between two files
    Why { a: String, b: String },
    /// Birth, renames, edge events, churn by period
    Timeline { file: String },
    /// Docs whose referenced code moved on without them
    Drift {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Files ranked by churn, coupling, or ghost involvement
    Hotspots {
        #[arg(long, default_value = "churn", value_parser = ["churn", "coupling", "ghosts"])]
        by: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Full-text search over commit messages
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Index health: coverage, unresolved-edge ratio, excluded share
    Stats,
    /// Dump the graph for external tools
    Export {
        #[arg(long, default_value = "json", value_parser = ["json", "dot", "graphml"])]
        format: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            let (code, msg) = classify_error(&e);
            eprintln!("pal: {msg}");
            code
        }
    };
    std::process::exit(code);
}

fn classify_error(e: &anyhow::Error) -> (i32, String) {
    if let Some(q) = e.downcast_ref::<QueryError>() {
        return match q {
            QueryError::FileNotFound(_) => (3, q.to_string()),
            QueryError::Other(inner) => (1, inner.to_string()),
        };
    }
    if let Some(s) = e.downcast_ref::<StoreError>() {
        return match s {
            StoreError::Missing(_) => (2, s.to_string()),
            StoreError::SchemaMismatch { .. } => (4, s.to_string()),
            _ => (1, s.to_string()),
        };
    }
    (1, format!("{e:#}"))
}

fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Index {
            path,
            since,
            incremental,
            all_parents,
            max_commit_files,
            half_life,
            jobs,
            quiet,
        } => {
            if let Some(n) = jobs {
                rayon_pool(*n);
            }
            let repo = path.clone().unwrap_or_else(|| PathBuf::from("."));
            let db = cli.db.clone().unwrap_or_else(|| repo.join(".pal/index.db"));
            let params = Params {
                max_commit_files: *max_commit_files,
                half_life_days: parse_days(half_life)?,
                ..Params::default()
            };
            let opts = IndexOptions {
                repo: repo.clone(),
                db: db.clone(),
                since: since.clone(),
                incremental: *incremental,
                all_parents: *all_parents,
                params: params.clone(),
                quiet: *quiet || cli.json,
            };
            let report = pal_index::walker::index(&opts)?;
            let store = Store::open(&db)?;
            let co = pal_analyze::cochange::compute(&store, &params)?;
            let gh = pal_analyze::ghosts::compute(&store, &params)?;
            if cli.json {
                let out = serde_json::json!({
                    "schema": query::SCHEMA_VERSION,
                    "commits_indexed": report.commits_indexed,
                    "files_total": report.files_total,
                    "imports_total": report.imports_total,
                    "imports_resolved": report.imports_resolved,
                    "cochange_pairs": co.pairs_written,
                    "ghosts": gh.ghosts,
                    "head": report.head,
                    "elapsed_secs": report.elapsed_secs,
                });
                println!("{}", serde_json::to_string(&out)?);
            } else {
                println!(
                    "indexed {} commits, {} files in {:.1}s",
                    report.commits_indexed, report.files_total, report.elapsed_secs
                );
                println!(
                    "imports resolved {}/{}, cochange pairs {}, ghosts {}",
                    report.imports_resolved, report.imports_total, co.pairs_written, gh.ghosts
                );
            }
            Ok(())
        }
        cmd => run_query(&cli, cmd),
    }
}

fn run_query(cli: &Cli, cmd: &Command) -> Result<()> {
    let db = find_db(cli.db.as_deref())?;
    let store = Store::open(&db)?;
    let params = load_params(&store);
    let ctx = query::Ctx::load(&store, params)?;
    let caveats = staleness_caveats(&store, &db);

    match cmd {
        Command::Blast {
            file,
            depth,
            limit,
            min_confidence,
            kinds,
        } => {
            let opts = BlastOptions {
                depth: *depth,
                limit: *limit,
                min_confidence: *min_confidence,
                kinds: kinds.as_deref().map(KindFilter::parse).unwrap_or_default(),
            };
            let mut out = query::blast(&store, &ctx, file, &opts)?;
            out.caveats.extend(caveats.clone());
            emit(cli, &out, human::blast)?;
        }
        Command::Ghosts { file, limit } => {
            let out = query::ghosts_list(&store, &ctx, file.as_deref(), *limit)?;
            emit_with_caveats(cli, &out, &caveats, human::ghosts)?;
        }
        Command::Cochange {
            file,
            limit,
            include_structural,
        } => {
            let out = query::cochange_list(&store, &ctx, file, *limit, *include_structural)?;
            emit_with_caveats(cli, &out, &caveats, human::cochange)?;
        }
        Command::Path { a, b, as_of } => {
            let out = query::path_query(&store, &ctx, a, b, as_of.as_deref())?;
            emit_with_caveats(cli, &out, &caveats, human::path)?;
        }
        Command::Why { a, b } => {
            let out = query::why(&store, &ctx, a, b)?;
            emit_with_caveats(cli, &out, &caveats, human::why)?;
        }
        Command::Timeline { file } => {
            let out = query::timeline(&store, &ctx, file)?;
            emit_with_caveats(cli, &out, &caveats, human::timeline)?;
        }
        Command::Drift { limit } => {
            let out = query::drift(&store, &ctx, *limit)?;
            emit_with_caveats(cli, &out, &caveats, human::drift)?;
        }
        Command::Hotspots { by, limit } => {
            let out = query::hotspots(&store, &ctx, by, *limit)?;
            emit_with_caveats(cli, &out, &caveats, human::hotspots)?;
        }
        Command::Search { query: q, limit } => {
            let out = query::search(&store, &ctx, q, *limit)?;
            emit_with_caveats(cli, &out, &caveats, human::search)?;
        }
        Command::Stats => {
            let out = query::stats(&store)?;
            emit_with_caveats(cli, &out, &caveats, human::stats)?;
        }
        Command::Export { format } => {
            export(&store, format)?;
        }
        Command::Index { .. } => unreachable!(),
    }
    Ok(())
}

/// Serialize as compact JSON with a caveats array injected, or render for
/// humans.
fn emit_with_caveats<T: serde::Serialize>(
    cli: &Cli,
    out: &T,
    caveats: &[String],
    render: impl Fn(&T),
) -> Result<()> {
    if cli.json {
        let mut v = serde_json::to_value(out)?;
        if let Some(obj) = v.as_object_mut() {
            if !obj.contains_key("caveats") {
                obj.insert("caveats".into(), serde_json::to_value(caveats)?);
            }
        }
        println!("{}", serde_json::to_string(&v)?);
    } else {
        render(out);
        for c in caveats {
            eprintln!("note: {c}");
        }
    }
    Ok(())
}

fn emit<T: serde::Serialize>(cli: &Cli, out: &T, render: impl Fn(&T)) -> Result<()> {
    if cli.json {
        println!("{}", serde_json::to_string(out)?);
    } else {
        render(out);
    }
    Ok(())
}

fn rayon_pool(n: usize) {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .build_global();
}

fn parse_days(s: &str) -> Result<f64> {
    let t = s.trim().trim_end_matches('d');
    t.parse::<f64>()
        .map_err(|_| anyhow::anyhow!("bad duration: {s} (expected e.g. 365d)"))
}

fn load_params(store: &Store) -> Params {
    let mut p = Params::default();
    if let Ok(Some(v)) = store.meta_get("max_commit_files") {
        if let Ok(n) = v.parse() {
            p.max_commit_files = n;
        }
    }
    if let Ok(Some(v)) = store.meta_get("half_life_days") {
        if let Ok(n) = v.parse() {
            p.half_life_days = n;
        }
    }
    p
}

/// Find the index db: explicit flag, then .pal/index.db walking up from cwd.
fn find_db(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    let mut dir = std::env::current_dir()?;
    loop {
        let cand = dir.join(".pal/index.db");
        if cand.exists() {
            return Ok(cand);
        }
        if !dir.pop() {
            return Err(StoreError::Missing(
                ".pal/index.db (searched up from current directory)".into(),
            )
            .into());
        }
    }
}

/// Compare the indexed head with the repository's current HEAD.
fn staleness_caveats(store: &Store, db: &Path) -> Vec<String> {
    let mut caveats = Vec::new();
    let repo_dir = db
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let (Ok(Some(indexed)), Ok(vcs)) = (store.meta_get("head_oid"), GitVcs::open(&repo_dir)) else {
        return caveats;
    };
    if let Ok(head) = vcs.head_oid() {
        let current = pal_store::full_oid(&head);
        if current != indexed {
            match vcs.commits(true, Some(&indexed), None) {
                Ok(missed) => caveats.push(format!(
                    "index is {} commits behind HEAD; run `pal index --incremental`",
                    missed.len()
                )),
                Err(_) => caveats.push(
                    "index head does not match repository HEAD; run `pal index --incremental`"
                        .to_string(),
                ),
            }
        }
    }
    caveats
}

fn export(store: &Store, format: &str) -> Result<()> {
    let files = store.all_files()?;
    let edges = store.all_edges()?;
    let intervals = store.all_intervals()?;
    let mut open: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for iv in &intervals {
        if iv.died.is_none() {
            open.insert(iv.edge_id);
        }
    }
    let ghosts: std::collections::HashSet<i64> =
        store.all_ghosts()?.into_iter().map(|g| g.edge_id).collect();
    match format {
        "dot" => {
            println!("digraph palimpsest {{");
            for f in files.iter().filter(|f| f.current_path.is_some()) {
                println!(
                    "  n{} [label=\"{}\"];",
                    f.id.0,
                    f.current_path.as_deref().unwrap()
                );
            }
            for e in &edges {
                let alive = open.contains(&e.id);
                let ghost = ghosts.contains(&e.id);
                if !alive && !ghost {
                    continue;
                }
                let style = if ghost { "dashed" } else { "solid" };
                println!(
                    "  n{} -> n{} [style={style} label=\"{}\"];",
                    e.src.0,
                    e.dst.0,
                    e.kind.as_str()
                );
            }
            println!("}}");
        }
        "graphml" => {
            println!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            println!("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">");
            println!("<key id=\"path\" for=\"node\" attr.name=\"path\" attr.type=\"string\"/>");
            println!("<key id=\"kind\" for=\"edge\" attr.name=\"kind\" attr.type=\"string\"/>");
            println!("<key id=\"state\" for=\"edge\" attr.name=\"state\" attr.type=\"string\"/>");
            println!("<graph edgedefault=\"directed\">");
            for f in files.iter().filter(|f| f.current_path.is_some()) {
                println!(
                    "  <node id=\"n{}\"><data key=\"path\">{}</data></node>",
                    f.id.0,
                    xml_escape(f.current_path.as_deref().unwrap())
                );
            }
            for e in &edges {
                let alive = open.contains(&e.id);
                let ghost = ghosts.contains(&e.id);
                if !alive && !ghost {
                    continue;
                }
                let state = if ghost { "ghost" } else { "live" };
                println!(
                    "  <edge source=\"n{}\" target=\"n{}\"><data key=\"kind\">{}</data><data key=\"state\">{state}</data></edge>",
                    e.src.0,
                    e.dst.0,
                    e.kind.as_str()
                );
            }
            println!("</graph></graphml>");
        }
        _ => {
            let nodes: Vec<serde_json::Value> = files
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "id": f.id.0,
                        "path": f.current_path.clone().unwrap_or_else(|| {
                            store.display_path(f.id).unwrap_or_default()
                        }),
                        "alive": f.current_path.is_some(),
                        "lang": f.lang,
                        "is_doc": f.is_doc,
                    })
                })
                .collect();
            let edge_vals: Vec<serde_json::Value> = edges
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "src": e.src.0,
                        "dst": e.dst.0,
                        "kind": e.kind.as_str(),
                        "resolution": e.resolution.as_str(),
                        "state": if open.contains(&e.id) { "live" } else if ghosts.contains(&e.id) { "ghost" } else { "dead" },
                    })
                })
                .collect();
            let cochange: Vec<serde_json::Value> = store
                .all_cochange()?
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "a": c.a.0, "b": c.b.0, "n": c.n,
                        "w_decayed": c.w_decayed, "lift": c.lift,
                    })
                })
                .collect();
            let out = serde_json::json!({
                "schema": query::SCHEMA_VERSION,
                "nodes": nodes,
                "edges": edge_vals,
                "cochange": cochange,
            });
            println!("{}", serde_json::to_string(&out)?);
        }
    }
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
