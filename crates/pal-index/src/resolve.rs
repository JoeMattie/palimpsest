//! The resolution ladder from plan section 3.3: relative path, config-aware
//! roots, unique symbol name, doc refs. Most-exact-first; ambiguity drops
//! the edge rather than guessing.

use crate::langs::Lang;
use pal_core::parsed::ParsedFile;
use pal_core::{EdgeKind, FileId, Resolution};
use std::collections::{BTreeSet, HashMap, HashSet};

pub struct ResolveCtx<'a> {
    /// Live repo-relative path -> file id.
    pub live: &'a HashMap<String, FileId>,
    /// Symbol name -> set of live files defining it.
    pub symbols: &'a HashMap<String, HashSet<FileId>>,
    /// Module path from go.mod, when present.
    pub go_module: Option<&'a str>,
    /// Rust crate name -> src root dir (from workspace Cargo.toml files).
    pub rust_crates: &'a HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct ResolvedEdges {
    pub edges: BTreeSet<(FileId, EdgeKind, Resolution)>,
    pub imports_total: u64,
    pub imports_resolved: u64,
}

pub fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

pub fn resolve_file(
    ctx: &ResolveCtx,
    src: FileId,
    src_path: &str,
    lang: Lang,
    parsed: &ParsedFile,
) -> ResolvedEdges {
    let mut out = ResolvedEdges::default();
    if lang == Lang::Md {
        resolve_doc(ctx, src, src_path, parsed, &mut out);
        return out;
    }

    let mut symbol_candidates: BTreeSet<String> = BTreeSet::new();

    for import in &parsed.imports {
        for n in &import.names {
            let last = n.rsplit('.').next().unwrap_or(n);
            if last.len() >= 3 {
                symbol_candidates.insert(last.to_string());
            }
        }
        if import.spec.is_empty() {
            continue;
        }
        let kind = if import.reexport {
            EdgeKind::Reexport
        } else {
            EdgeKind::Import
        };
        let spec = import.spec.as_str();
        // Only internal-shaped specifiers count toward the resolution health
        // ratio; a bare `torch` or `react` that stays unresolved is an
        // external dependency, not an indexing failure.
        let internal_shaped = match lang {
            Lang::Ts | Lang::Tsx | Lang::Js | Lang::Py => spec.starts_with('.'),
            Lang::Rust => {
                spec.starts_with("mod:")
                    || matches!(
                        spec.split("::").next(),
                        Some("crate") | Some("self") | Some("super")
                    )
                    || spec
                        .split("::")
                        .next()
                        .is_some_and(|first| ctx.rust_crates.contains_key(&first.replace('-', "_")))
            }
            Lang::Go => {
                spec.starts_with("./")
                    || ctx
                        .go_module
                        .is_some_and(|m| spec == m || spec.starts_with(&format!("{m}/")))
            }
            Lang::Md => false,
        };
        let target = match lang {
            Lang::Ts | Lang::Tsx | Lang::Js => resolve_js(ctx, src_path, spec),
            Lang::Py => resolve_py(ctx, src_path, spec),
            Lang::Rust => resolve_rust(ctx, src_path, spec),
            Lang::Go => {
                let targets = resolve_go(ctx, src_path, spec);
                if internal_shaped {
                    out.imports_total += 1;
                    if !targets.is_empty() {
                        out.imports_resolved += 1;
                    }
                }
                for t in targets {
                    if t != src {
                        out.edges.insert((t, kind, Resolution::Heuristic));
                    }
                }
                continue;
            }
            Lang::Md => None,
        };
        // Resolution of a bare specifier against the repo tree still counts:
        // it turned out to be internal after all.
        if internal_shaped || target.is_some() {
            out.imports_total += 1;
        }
        match target {
            Some((t, res)) => {
                out.imports_resolved += 1;
                if t != src {
                    out.edges.insert((t, kind, res));
                }
            }
            None => {
                // Unique-symbol fallback on the last path segment.
                let last = spec
                    .rsplit([':', '.', '/'])
                    .next()
                    .unwrap_or("");
                if last.len() >= 3 && last != "*" {
                    if let Some(t) = unique_symbol(ctx, src, last) {
                        if internal_shaped {
                            out.imports_resolved += 1;
                        }
                        out.edges.insert((t, kind, Resolution::SymbolName));
                    }
                }
            }
        }
    }

    // Imported names, call sites, and type references through unique symbols.
    for name in symbol_candidates {
        if let Some(t) = unique_symbol(ctx, src, &name) {
            out.edges
                .insert((t, EdgeKind::Import, Resolution::SymbolName));
        }
    }
    for call in &parsed.calls {
        if let Some(t) = unique_symbol(ctx, src, call) {
            out.edges
                .insert((t, EdgeKind::Call, Resolution::SymbolName));
        }
    }
    for tr in &parsed.type_refs {
        if let Some(t) = unique_symbol(ctx, src, tr) {
            out.edges
                .insert((t, EdgeKind::TypeRef, Resolution::SymbolName));
        }
    }
    out
}

fn unique_symbol(ctx: &ResolveCtx, src: FileId, name: &str) -> Option<FileId> {
    let set = ctx.symbols.get(name)?;
    let mut others = set.iter().filter(|f| **f != src);
    let first = *others.next()?;
    if others.next().is_some() {
        // Ambiguous: drop, do not guess.
        return None;
    }
    Some(first)
}

fn resolve_js(ctx: &ResolveCtx, src_path: &str, spec: &str) -> Option<(FileId, Resolution)> {
    const EXTS: &[&str] = &[".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs"];
    const INDEXES: &[&str] = &["index.ts", "index.tsx", "index.js", "index.jsx"];
    let try_base = |base: &str, res: Resolution| -> Option<(FileId, Resolution)> {
        let base = normalize_path(base);
        if let Some(id) = ctx.live.get(&base) {
            return Some((*id, res));
        }
        // ESM specifiers name the emitted .js while the source is .ts.
        for (from, to) in [
            (".js", ".ts"),
            (".jsx", ".tsx"),
            (".mjs", ".mts"),
            (".cjs", ".cts"),
        ] {
            if let Some(stem) = base.strip_suffix(from) {
                if let Some(id) = ctx.live.get(&format!("{stem}{to}")) {
                    return Some((*id, res));
                }
            }
        }
        for ext in EXTS {
            if let Some(id) = ctx.live.get(&format!("{base}{ext}")) {
                return Some((*id, res));
            }
        }
        for ix in INDEXES {
            if let Some(id) = ctx.live.get(&join(&base, ix)) {
                return Some((*id, res));
            }
        }
        None
    };
    if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
        return try_base(&join(dir_of(src_path), spec), Resolution::PathExact);
    }
    // Bare specifier: try repo-root resolution and a conventional src/ root
    // before giving up. Externals fall through to None and are dropped.
    if let Some(hit) = try_base(spec, Resolution::Heuristic) {
        return Some(hit);
    }
    try_base(&format!("src/{spec}"), Resolution::Heuristic)
}

fn resolve_py(ctx: &ResolveCtx, src_path: &str, spec: &str) -> Option<(FileId, Resolution)> {
    let try_base = |base: &str, res: Resolution| -> Option<(FileId, Resolution)> {
        let base = normalize_path(base);
        if base.is_empty() {
            return None;
        }
        if let Some(id) = ctx.live.get(&format!("{base}.py")) {
            return Some((*id, res));
        }
        if let Some(id) = ctx.live.get(&join(&base, "__init__.py")) {
            return Some((*id, res));
        }
        None
    };
    if let Some(stripped) = spec.strip_prefix('.') {
        // Relative import: one dot is the current package, each further dot
        // goes up one package.
        let extra_dots = stripped.chars().take_while(|c| *c == '.').count();
        let rest = &stripped[extra_dots..];
        let mut dir = dir_of(src_path).to_string();
        for _ in 0..extra_dots {
            dir = dir_of(&dir).to_string();
        }
        if rest.is_empty() {
            // "from . import x": the package itself.
            return ctx
                .live
                .get(&join(&dir, "__init__.py"))
                .map(|id| (*id, Resolution::PathExact));
        }
        let rel = rest.replace('.', "/");
        return try_base(&join(&dir, &rel), Resolution::PathExact);
    }
    let path_form = spec.replace('.', "/");
    // Absolute imports: repo root, then src/, then the importing file's own
    // topmost package root.
    if let Some(hit) = try_base(&path_form, Resolution::PathExact) {
        return Some(hit);
    }
    if let Some(hit) = try_base(&format!("src/{path_form}"), Resolution::PathExact) {
        return Some(hit);
    }
    if let Some(root) = python_package_root(ctx, src_path) {
        if let Some(hit) = try_base(&join(&root, &path_form), Resolution::Heuristic) {
            return Some(hit);
        }
    }
    None
}

/// The directory above the topmost package containing this file, judged by
/// the presence of __init__.py files in the live tree.
fn python_package_root(ctx: &ResolveCtx, src_path: &str) -> Option<String> {
    let mut dir = dir_of(src_path).to_string();
    let mut root = None;
    while !dir.is_empty() {
        if ctx.live.contains_key(&join(&dir, "__init__.py")) {
            root = Some(dir_of(&dir).to_string());
            dir = dir_of(&dir).to_string();
        } else {
            break;
        }
    }
    root
}

fn resolve_rust(ctx: &ResolveCtx, src_path: &str, spec: &str) -> Option<(FileId, Resolution)> {
    let dir = dir_of(src_path);
    if let Some(name) = spec.strip_prefix("mod:") {
        let a = join(dir, &format!("{name}.rs"));
        let b = join(dir, &format!("{name}/mod.rs"));
        // A mod declared in foo.rs looks in foo/ when there is no sibling.
        let stem = crate::vcs::file_stem(src_path);
        let c = join(dir, &format!("{stem}/{name}.rs"));
        for cand in [a, b, c] {
            if let Some(id) = ctx.live.get(&normalize_path(&cand)) {
                return Some((*id, Resolution::PathExact));
            }
        }
        return None;
    }
    let segs: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    let mut crate_rooted = false;
    let (root, body) = match segs[0] {
        "crate" => {
            crate_rooted = true;
            (rust_crate_root(ctx, src_path)?, &segs[1..])
        }
        "self" => (dir.to_string(), &segs[1..]),
        "super" => {
            let mut d = dir.to_string();
            let mut i = 0;
            while i < segs.len() && segs[i] == "super" {
                d = dir_of(&d).to_string();
                i += 1;
            }
            (d, &segs[i..])
        }
        "std" | "core" | "alloc" => return None,
        first => match ctx.rust_crates.get(&first.replace('-', "_")) {
            Some(root) => {
                crate_rooted = true;
                (root.clone(), &segs[1..])
            }
            None => return None,
        },
    };
    // Try the longest prefix of the remaining segments as a module path.
    for take in (1..=body.len()).rev() {
        let prefix = body[..take].join("/");
        for cand in [
            join(&root, &format!("{prefix}.rs")),
            join(&root, &format!("{prefix}/mod.rs")),
        ] {
            let cand = normalize_path(&cand);
            if cand == src_path {
                continue;
            }
            if let Some(id) = ctx.live.get(&cand) {
                return Some((*id, Resolution::PathExact));
            }
        }
    }
    // use crate::Foo (or other_crate::Foo) resolves to that crate's root file.
    if crate_rooted && body.len() <= 1 {
        for cand in [join(&root, "lib.rs"), join(&root, "main.rs")] {
            if cand != src_path {
                if let Some(id) = ctx.live.get(&cand) {
                    return Some((*id, Resolution::PathExact));
                }
            }
        }
    }
    None
}

fn rust_crate_root(ctx: &ResolveCtx, src_path: &str) -> Option<String> {
    let mut dir = dir_of(src_path).to_string();
    loop {
        if ctx.live.contains_key(&join(&dir, "lib.rs"))
            || ctx.live.contains_key(&join(&dir, "main.rs"))
        {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir_of(&dir).to_string();
    }
}

fn resolve_go(ctx: &ResolveCtx, src_path: &str, spec: &str) -> Vec<FileId> {
    let rel_dir = if let Some(module) = ctx.go_module {
        if spec == module {
            Some(String::new())
        } else {
            spec.strip_prefix(&format!("{module}/")).map(String::from)
        }
    } else {
        None
    };
    let rel_dir = match rel_dir {
        Some(d) => d,
        None => {
            if spec.starts_with("./") {
                normalize_path(&join(dir_of(src_path), spec))
            } else {
                return Vec::new();
            }
        }
    };
    // Package import: edge to each direct .go file in the directory.
    let mut out: Vec<FileId> = Vec::new();
    let prefix = if rel_dir.is_empty() {
        String::new()
    } else {
        format!("{rel_dir}/")
    };
    for (path, id) in ctx.live.iter() {
        if !path.ends_with(".go") || path.ends_with("_test.go") {
            continue;
        }
        if let Some(rest) = path.strip_prefix(&prefix) {
            if !rest.contains('/') {
                out.push(*id);
                if out.len() >= 20 {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}

fn resolve_doc(
    ctx: &ResolveCtx,
    src: FileId,
    src_path: &str,
    parsed: &ParsedFile,
    out: &mut ResolvedEdges,
) {
    for link in &parsed.doc_links {
        out.imports_total += 1;
        let candidates = [
            normalize_path(&join(dir_of(src_path), link)),
            normalize_path(link),
        ];
        let mut hit = false;
        for cand in candidates {
            if cand.is_empty() {
                continue;
            }
            if let Some(id) = ctx.live.get(&cand) {
                if *id != src {
                    out.edges
                        .insert((*id, EdgeKind::DocRef, Resolution::PathExact));
                }
                hit = true;
                break;
            }
        }
        if hit {
            out.imports_resolved += 1;
        }
    }
    for tok in &parsed.doc_tokens {
        if let Some(t) = unique_symbol(ctx, src, tok) {
            out.edges
                .insert((t, EdgeKind::DocRef, Resolution::Heuristic));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_fixture() -> (HashMap<String, FileId>, HashMap<String, HashSet<FileId>>) {
        let mut live = HashMap::new();
        live.insert("src/encoder.ts".to_string(), FileId(1));
        live.insert("src/frame.ts".to_string(), FileId(2));
        live.insert("src/util/index.ts".to_string(), FileId(3));
        live.insert("pkg/__init__.py".to_string(), FileId(4));
        live.insert("pkg/frame.py".to_string(), FileId(5));
        live.insert("pkg/sub/enc.py".to_string(), FileId(6));
        live.insert("src/lib.rs".to_string(), FileId(7));
        live.insert("src/codec/mod.rs".to_string(), FileId(8));
        live.insert("src/codec/frame.rs".to_string(), FileId(9));
        let mut symbols: HashMap<String, HashSet<FileId>> = HashMap::new();
        symbols.insert("FrameCodec".to_string(), HashSet::from([FileId(2)]));
        symbols.insert(
            "ambiguous".to_string(),
            HashSet::from([FileId(2), FileId(3)]),
        );
        (live, symbols)
    }

    #[test]
    fn js_relative_and_index() {
        let (live, symbols) = ctx_fixture();
        let rc = HashMap::new();
        let ctx = ResolveCtx {
            live: &live,
            symbols: &symbols,
            go_module: None,
            rust_crates: &rc,
        };
        assert_eq!(
            resolve_js(&ctx, "src/encoder.ts", "./frame"),
            Some((FileId(2), Resolution::PathExact))
        );
        assert_eq!(
            resolve_js(&ctx, "src/encoder.ts", "./util"),
            Some((FileId(3), Resolution::PathExact))
        );
        assert_eq!(resolve_js(&ctx, "src/encoder.ts", "react"), None);
    }

    #[test]
    fn py_relative_and_absolute() {
        let (live, symbols) = ctx_fixture();
        let rc = HashMap::new();
        let ctx = ResolveCtx {
            live: &live,
            symbols: &symbols,
            go_module: None,
            rust_crates: &rc,
        };
        assert_eq!(
            resolve_py(&ctx, "pkg/sub/enc.py", "..frame"),
            Some((FileId(5), Resolution::PathExact))
        );
        assert_eq!(
            resolve_py(&ctx, "pkg/sub/enc.py", "pkg.frame"),
            Some((FileId(5), Resolution::PathExact))
        );
        assert_eq!(resolve_py(&ctx, "pkg/sub/enc.py", "numpy"), None);
    }

    #[test]
    fn rust_crate_paths() {
        let (live, symbols) = ctx_fixture();
        let rc = HashMap::new();
        let ctx = ResolveCtx {
            live: &live,
            symbols: &symbols,
            go_module: None,
            rust_crates: &rc,
        };
        assert_eq!(
            resolve_rust(&ctx, "src/main.rs", "crate::codec::frame::Frame"),
            Some((FileId(9), Resolution::PathExact))
        );
        assert_eq!(resolve_rust(&ctx, "src/codec/frame.rs", "super::mod"), None);
        assert_eq!(
            resolve_rust(&ctx, "src/lib.rs", "mod:codec"),
            Some((FileId(8), Resolution::PathExact))
        );
    }

    #[test]
    fn ambiguous_symbol_dropped() {
        let (live, symbols) = ctx_fixture();
        let rc = HashMap::new();
        let ctx = ResolveCtx {
            live: &live,
            symbols: &symbols,
            go_module: None,
            rust_crates: &rc,
        };
        assert_eq!(
            unique_symbol(&ctx, FileId(1), "FrameCodec"),
            Some(FileId(2))
        );
        assert_eq!(unique_symbol(&ctx, FileId(1), "ambiguous"), None);
        // The only definer being the asker itself resolves to nothing.
        assert_eq!(unique_symbol(&ctx, FileId(2), "FrameCodec"), None);
    }
}
