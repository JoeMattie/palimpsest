//! `pal serve`: local HTTP server for the blast-graph visualizer.
//!
//! Serves a single self-contained page (d3-force inlined, no external
//! requests) with the graph data injected as JSON. Data is recomputed on
//! every request, so a re-index shows up on reload.

use anyhow::Result;
use pal_store::Store;
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;

const TEMPLATE: &str = include_str!("../assets/viz.html");

/// Top-level dirs treated as monorepo containers: files under them group by
/// the second path component instead of the first.
const CONTAINERS: &[&str] = &["packages", "apps", "crates", "libs", "services", "modules"];

/// The page styles exactly 8 group colors; the last is the catch-all.
const MAX_GROUPS: usize = 8;
const MIN_LIFT: f64 = 2.0;
const HIDDEN_MIN_LIFT: f64 = 8.0;
const HIDDEN_MIN_N: i64 = 3;
const HIDDEN_LIMIT: usize = 40;

pub fn serve(store: &Store, db: &Path, port: u16) -> Result<()> {
    let repo_name = db
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let url = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    eprintln!("pal: serving {url} (ctrl-c to stop)");

    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req.split_whitespace().nth(1).unwrap_or("/");
        let (status, ctype, body) = match target {
            "/" | "/index.html" => match render(store, &repo_name) {
                Ok(html) => ("200 OK", "text/html; charset=utf-8", html),
                Err(e) => ("500 Internal Server Error", "text/plain", format!("pal: {e:#}")),
            },
            "/data.json" => match viz_data(store, &repo_name) {
                Ok(v) => ("200 OK", "application/json", v.to_string()),
                Err(e) => ("500 Internal Server Error", "text/plain", format!("pal: {e:#}")),
            },
            _ => ("404 Not Found", "text/plain", "not found".into()),
        };
        // the page is fully self-contained, so everything but inline
        // script/style can be denied outright
        let _ = write!(
            s,
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    Ok(())
}

fn render(store: &Store, repo_name: &str) -> Result<String> {
    let data = viz_data(store, repo_name)?.to_string();
    // "</" cannot appear raw inside the inline <script> data block
    Ok(TEMPLATE.replace("__DATA__", &data.replace("</", "<\\/")))
}

/// Group label for a path: first component, or the first two under a
/// monorepo container dir, or "(root)" for top-level files.
fn group_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2 {
        "(root)".into()
    } else if parts.len() > 2 && CONTAINERS.contains(&parts[0]) {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].into()
    }
}

/// The compact payload the page consumes. Nodes are files that participate
/// in at least one live structural pair or qualifying co-change pair;
/// arrays index into the node list.
pub fn viz_data(store: &Store, repo_name: &str) -> Result<serde_json::Value> {
    let files = store.all_files()?;
    let alive: HashMap<i64, &str> = files
        .iter()
        .filter_map(|f| f.current_path.as_deref().map(|p| (f.id.0, p)))
        .collect();

    let mut open: HashSet<i64> = HashSet::new();
    for iv in store.all_intervals()? {
        if iv.died.is_none() {
            open.insert(iv.edge_id);
        }
    }
    let mut live_pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
    for e in store.all_edges()? {
        if open.contains(&e.id) && alive.contains_key(&e.src.0) && alive.contains_key(&e.dst.0) {
            let (a, b) = if e.src.0 <= e.dst.0 {
                (e.src.0, e.dst.0)
            } else {
                (e.dst.0, e.src.0)
            };
            live_pairs.insert((a, b));
        }
    }

    let mut co: Vec<pal_store::CochangeRow> = store
        .all_cochange()?
        .into_iter()
        .filter(|c| c.lift >= MIN_LIFT && alive.contains_key(&c.a.0) && alive.contains_key(&c.b.0))
        .collect();
    co.sort_by(|x, y| y.w_decayed.total_cmp(&x.w_decayed));

    let mut used: BTreeSet<i64> = BTreeSet::new();
    for (a, b) in &live_pairs {
        used.insert(*a);
        used.insert(*b);
    }
    for c in &co {
        used.insert(c.a.0);
        used.insert(c.b.0);
    }
    let ids: Vec<i64> = used.into_iter().collect();
    let ix: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    // groups ranked by member count; lump the tail into "other" only when
    // there are more groups than color slots
    let mut group_count: HashMap<String, usize> = HashMap::new();
    for id in &ids {
        *group_count.entry(group_of(alive[id])).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = group_count.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut groups: Vec<String> = ranked.iter().map(|(g, _)| g.clone()).collect();
    if groups.len() > MAX_GROUPS {
        groups.truncate(MAX_GROUPS - 1);
        groups.push("other".into());
    }
    let gid = |p: &str| {
        let g = group_of(p);
        groups.iter().position(|x| *x == g).unwrap_or(groups.len() - 1)
    };

    let mut deg: HashMap<i64, i64> = HashMap::new();
    for (a, b) in &live_pairs {
        *deg.entry(*a).or_default() += 1;
        *deg.entry(*b).or_default() += 1;
    }
    let mut costr: HashMap<i64, f64> = HashMap::new();
    for c in &co {
        *costr.entry(c.a.0).or_default() += c.w_decayed;
        *costr.entry(c.b.0).or_default() += c.w_decayed;
    }

    let nodes: Vec<serde_json::Value> = ids
        .iter()
        .map(|n| {
            let p = alive[n];
            json!([p, gid(p), deg.get(n).copied().unwrap_or(0), round2(*costr.get(n).unwrap_or(&0.0))])
        })
        .collect();
    let live: Vec<serde_json::Value> =
        live_pairs.iter().map(|(a, b)| json!([ix[a], ix[b]])).collect();
    let co_rows: Vec<serde_json::Value> = co
        .iter()
        .map(|c| json!([ix[&c.a.0], ix[&c.b.0], round1(c.lift), c.n, round3(c.w_decayed)]))
        .collect();
    let live_ix: HashSet<(usize, usize)> = live_pairs
        .iter()
        .map(|(a, b)| (ix[a].min(ix[b]), ix[a].max(ix[b])))
        .collect();
    let hidden: Vec<serde_json::Value> = co
        .iter()
        .filter(|c| {
            let (a, b) = (ix[&c.a.0], ix[&c.b.0]);
            !live_ix.contains(&(a.min(b), a.max(b)))
                && c.lift >= HIDDEN_MIN_LIFT
                && c.n >= HIDDEN_MIN_N
        })
        .take(HIDDEN_LIMIT)
        .map(|c| json!([ix[&c.a.0], ix[&c.b.0], round1(c.lift), c.n, round3(c.w_decayed)]))
        .collect();

    let head = store
        .meta_get("head_oid")?
        .map(|h| h.chars().take(7).collect::<String>());
    Ok(json!({
        "meta": {
            "repo": repo_name,
            "head": head,
            "commits": store.all_commits()?.len(),
            "files": alive.len(),
            "live": live.len(),
            "cochange": co_rows.len(),
        },
        "groups": groups,
        "nodes": nodes,
        "live": live,
        "co": co_rows,
        "hidden": hidden,
    }))
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_core::{EdgeKind, Resolution};
    use pal_store::CochangeRow;

    #[test]
    fn group_heuristic() {
        assert_eq!(group_of("README.md"), "(root)");
        assert_eq!(group_of("src/main.rs"), "src");
        assert_eq!(group_of("packages/schemas/src/book.ts"), "packages/schemas");
        assert_eq!(group_of("packages/loose.ts"), "packages");
    }

    #[test]
    fn viz_data_shape() {
        let store = Store::in_memory().unwrap();
        let c = store
            .insert_commit(&[1; 20], None, 1000, "a", "s", "", 2, 1.0, 0, false)
            .unwrap();
        let fa = store.insert_file("src/a.rs", Some("rust"), c, false).unwrap();
        let fb = store.insert_file("src/b.rs", Some("rust"), c, false).unwrap();
        let fc = store.insert_file("docs/c.md", Some("markdown"), c, true).unwrap();
        let e = store
            .edge_get_or_create(fa, fb, EdgeKind::Import, Resolution::PathExact)
            .unwrap();
        store.interval_open(e, c).unwrap();
        store
            .cochange_insert(&CochangeRow {
                a: fa,
                b: fc,
                n: 5,
                w_support: 4.0,
                w_decayed: 3.5,
                conf_ab: 0.8,
                conf_ba: 0.7,
                lift: 9.0,
                first_commit: c,
                last_commit: c,
            })
            .unwrap();

        let v = viz_data(&store, "demo").unwrap();
        assert_eq!(v["meta"]["repo"], "demo");
        assert_eq!(v["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(v["live"].as_array().unwrap().len(), 1);
        assert_eq!(v["co"].as_array().unwrap().len(), 1);
        // co-change pair with no live edge, lift >= 8, n >= 3: hidden coupling
        assert_eq!(v["hidden"].as_array().unwrap().len(), 1);
        let groups: Vec<&str> = v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g.as_str().unwrap())
            .collect();
        assert!(groups.contains(&"src") && groups.contains(&"docs"));
    }

    #[test]
    fn template_has_placeholder() {
        assert!(TEMPLATE.contains("__DATA__"));
        assert_eq!(TEMPLATE.matches("__DATA__").count(), 1);
    }
}
