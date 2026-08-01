//! End-to-end tests against a programmatically built fixture repo covering
//! the plan's canonical scenarios: rename chains, delete/re-add, the
//! interface-extraction ghost, and a lint-storm commit.

use std::path::Path;
use std::process::Command;

fn git(repo: &Path, date: &str, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .env("GIT_AUTHOR_DATE", format!("{date}T12:00:00"))
        .env("GIT_COMMITTER_DATE", format!("{date}T12:00:00"))
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(repo: &Path, rel: &str, content: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn append(repo: &Path, rel: &str, line: &str) {
    let p = repo.join(rel);
    let mut c = std::fs::read_to_string(&p).unwrap_or_default();
    c.push_str(line);
    c.push('\n');
    std::fs::write(p, c).unwrap();
}

fn commit(repo: &Path, date: &str, msg: &str) {
    git(repo, date, &["add", "-A"]);
    git(repo, date, &["commit", "-q", "-m", msg]);
}

fn pal(repo: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pal"))
        .current_dir(repo)
        .args(args)
        .output()
        .expect("pal");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn build_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, "2024-01-01", &["init", "-q"]);
    git(repo, "2024-01-01", &["config", "user.email", "f@x.com"]);
    git(repo, "2024-01-01", &["config", "user.name", "Fixture"]);

    write(
        repo,
        "src/frame.ts",
        "export interface Frame { data: Uint8Array }\nexport function makeFrame(): Frame { return { data: new Uint8Array() } }\n",
    );
    write(
        repo,
        "src/encoder.ts",
        "import { Frame } from \"./frame\";\nexport function encode(f: Frame) { return f.data }\n",
    );
    write(
        repo,
        "src/parser.ts",
        "export function parse(s: string) { return s.length }\n",
    );
    write(repo, "test/golden/basic.txt", "golden v1\n");
    commit(repo, "2024-01-10", "initial layout");

    // Live-import era co-changes.
    for d in ["2024-02-05", "2024-03-15", "2024-04-20", "2024-06-01"] {
        append(repo, "src/frame.ts", &format!("// {d}"));
        append(repo, "src/encoder.ts", &format!("// {d}"));
        commit(repo, d, &format!("evolve wire format {d}"));
    }
    // Parser and golden fixtures, no structural link.
    for d in [
        "2024-02-10",
        "2024-05-05",
        "2024-09-09",
        "2025-01-15",
        "2025-06-06",
    ] {
        append(repo, "src/parser.ts", &format!("// {d}"));
        append(repo, "test/golden/basic.txt", d);
        commit(repo, d, &format!("parser edge cases {d}"));
    }
    // Interface extraction: the canonical ghost.
    write(
        repo,
        "src/iface.ts",
        "export interface FrameLike { data: Uint8Array }\n",
    );
    write(
        repo,
        "src/encoder.ts",
        "import { FrameLike } from \"./iface\";\nexport function encode(f: FrameLike) { return f.data }\n",
    );
    commit(repo, "2024-08-15", "extract FrameLike iface to break cycle");
    // Post-severance co-changes.
    for d in [
        "2024-10-01",
        "2024-12-12",
        "2025-02-02",
        "2025-05-20",
        "2025-09-01",
    ] {
        append(repo, "src/frame.ts", &format!("// {d}"));
        append(repo, "src/encoder.ts", &format!("// {d}"));
        commit(repo, d, &format!("both sides change {d}"));
    }
    // Rename chain.
    std::fs::create_dir_all(repo.join("src/core")).unwrap();
    git(
        repo,
        "2025-10-01",
        &["mv", "src/frame.ts", "src/core/frame.ts"],
    );
    commit(repo, "2025-10-01", "move frame into core/");
    git(
        repo,
        "2025-10-15",
        &["mv", "src/core/frame.ts", "src/core/frame2.ts"],
    );
    commit(repo, "2025-10-15", "rename frame to frame2");
    // Delete then re-add.
    git(repo, "2025-11-01", &["rm", "-q", "src/parser.ts"]);
    commit(repo, "2025-11-01", "drop parser");
    write(
        repo,
        "src/parser.ts",
        "export function parse(s: string) { return s.length + 1 }\n",
    );
    commit(repo, "2025-11-20", "restore parser");
    // Lint storm: mechanical, must not sever into ghosts.
    for i in 1..=10 {
        write(
            repo,
            &format!("src/mod{i}.ts"),
            &format!("import {{ FrameLike }} from \"./iface\";\nexport const M{i} = {i};\n"),
        );
    }
    commit(repo, "2025-12-01", "add modules");
    for i in 1..=10 {
        write(
            repo,
            &format!("src/mod{i}.ts"),
            &format!("import type {{ FrameLike }} from \"./iface\";\nexport const M{i} = {i};\n"),
        );
    }
    commit(repo, "2025-12-15", "chore: reorder imports");

    let (code, _, err) = pal(repo, &["index", ".", "--quiet"]);
    assert_eq!(code, 0, "index failed: {err}");
    dir
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("valid json")
}

#[test]
fn ghost_survives_interface_extraction_and_renames() {
    let dir = build_fixture();
    let (code, out, _) = pal(dir.path(), &["ghosts", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    let results = v["results"].as_array().unwrap();
    assert!(!results.is_empty(), "expected at least one ghost");
    let g = results
        .iter()
        .find(|g| g["kind"] == "import")
        .expect("an import ghost");
    // Followed through two renames.
    assert_eq!(g["from"], "src/encoder.ts");
    assert_eq!(g["to"], "src/core/frame2.ts");
    assert!(g["cochanges_since"].as_i64().unwrap() >= 2);
    assert!(g["confidence_since"].as_f64().unwrap() >= 0.25);
    assert!(g["severed_at"]["subject"]
        .as_str()
        .unwrap()
        .contains("extract FrameLike"));
    // The lint storm must not have produced mod-file ghosts.
    for g in results {
        assert!(!g["from"].as_str().unwrap().contains("mod"));
    }
}

#[test]
fn blast_ranks_ghost_partner_first() {
    let dir = build_fixture();
    let (code, out, _) = pal(dir.path(), &["blast", "src/encoder.ts", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    assert_eq!(v["schema"], 1);
    let results = v["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["path"], "src/core/frame2.ts");
    let kinds: Vec<&str> = results[0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"ghost"));
    assert!(kinds.contains(&"cochange"));
}

#[test]
fn rename_chain_is_one_identity() {
    let dir = build_fixture();
    // The historical path resolves to the same file as the current one.
    let (code, out, _) = pal(dir.path(), &["timeline", "src/frame.ts", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    assert_eq!(v["file"], "src/core/frame2.ts");
    let paths: Vec<&str> = v["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["src/frame.ts", "src/core/frame.ts", "src/core/frame2.ts"]
    );
}

#[test]
fn delete_readd_resurrects_identity() {
    let dir = build_fixture();
    let (code, out, _) = pal(dir.path(), &["timeline", "src/parser.ts", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    // Born at the initial commit, not at the restore commit.
    assert!(v["born"]["subject"].as_str().unwrap().contains("initial"));
    assert!(v["died"].is_null());
}

#[test]
fn cochange_finds_fixture_coupling_with_lift() {
    let dir = build_fixture();
    let (code, out, _) = pal(dir.path(), &["cochange", "src/parser.ts", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    let results = v["results"].as_array().unwrap();
    let golden = results
        .iter()
        .find(|r| r["path"] == "test/golden/basic.txt")
        .expect("golden fixture coupling");
    assert!(golden["lift"].as_f64().unwrap() > 1.5);
    assert!(golden["n"].as_i64().unwrap() >= 3);
    assert_eq!(golden["structural_edge"], false);
}

#[test]
fn mechanical_commit_excluded_from_metrics() {
    let dir = build_fixture();
    let (code, out, _) = pal(dir.path(), &["stats", "--json"]);
    assert_eq!(code, 0);
    let v = json(&out);
    assert!(v["commits"]["excluded_mechanical"].as_i64().unwrap() >= 1);
}

#[test]
fn why_reports_severing_commit() {
    let dir = build_fixture();
    let (code, out, _) = pal(
        dir.path(),
        &["why", "src/encoder.ts", "src/core/frame2.ts", "--json"],
    );
    assert_eq!(code, 0);
    let v = json(&out);
    let edges = v["edges"].as_array().unwrap();
    assert!(!edges.is_empty());
    let ghost_edge = edges
        .iter()
        .find(|e| !e["ghost"].is_null())
        .expect("ghost detail");
    assert!(ghost_edge["ghost"]["severed_at"]["subject"]
        .as_str()
        .unwrap()
        .contains("extract"));
}

#[test]
fn path_reports_break_commit() {
    let dir = build_fixture();
    let (code, out, _) = pal(
        dir.path(),
        &["path", "src/encoder.ts", "src/core/frame2.ts", "--json"],
    );
    assert_eq!(code, 0);
    let v = json(&out);
    assert!(v["live_path"].is_null());
    assert!(!v["union_path"].is_null());
    assert!(v["broke_at"]["subject"]
        .as_str()
        .unwrap()
        .contains("extract"));
}

#[test]
fn exit_codes() {
    let dir = build_fixture();
    let (code, _, _) = pal(dir.path(), &["blast", "no/such/file.ts"]);
    assert_eq!(code, 3);
    let empty = tempfile::tempdir().unwrap();
    let (code, _, _) = pal(empty.path(), &["stats"]);
    assert_eq!(code, 2);
}

#[test]
fn incremental_extends_index() {
    let dir = build_fixture();
    let repo = dir.path();
    append(repo, "src/encoder.ts", "// incremental touch");
    commit(repo, "2026-01-05", "one more encoder change");
    let (code, out, err) = pal(repo, &["index", ".", "--incremental", "--quiet", "--json"]);
    assert_eq!(code, 0, "{err}");
    let v = json(&out);
    assert_eq!(v["commits_indexed"], 1);
    // No staleness caveat afterward.
    let (_, out, _) = pal(repo, &["blast", "src/encoder.ts", "--json"]);
    let v = json(&out);
    assert!(v["caveats"].as_array().unwrap().is_empty());
}
