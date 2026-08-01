//! Language detection and per-blob extraction. tree-sitter parses a single
//! file with no build environment, which is the only thing that works on
//! historical checkouts; see plan section 3.1. Markdown is handled with a
//! small hand extractor instead of a grammar.

use pal_core::parsed::{ImportRef, ParsedFile, SymbolDef};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::OnceLock;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Ts,
    Tsx,
    Js,
    Py,
    Rust,
    Go,
    Rb,
    Coffee,
    Md,
}

impl Lang {
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "ts" | "mts" | "cts" => Lang::Ts,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::Js,
            "py" | "pyi" => Lang::Py,
            "rs" => Lang::Rust,
            "go" => Lang::Go,
            "rb" | "rake" | "gemspec" => Lang::Rb,
            "coffee" | "cjsx" => Lang::Coffee,
            "md" | "markdown" | "mdx" => Lang::Md,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Ts => "ts",
            Lang::Tsx => "tsx",
            Lang::Js => "js",
            Lang::Py => "py",
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Rb => "rb",
            Lang::Coffee => "coffee",
            Lang::Md => "md",
        }
    }

    pub fn parse_str(s: &str) -> Option<Lang> {
        Some(match s {
            "ts" => Lang::Ts,
            "tsx" => Lang::Tsx,
            "js" => Lang::Js,
            "py" => Lang::Py,
            "rust" => Lang::Rust,
            "go" => Lang::Go,
            "rb" => Lang::Rb,
            "coffee" => Lang::Coffee,
            "md" => Lang::Md,
            _ => return None,
        })
    }

    pub fn is_doc(self) -> bool {
        matches!(self, Lang::Md)
    }

    fn language(self) -> Option<Language> {
        Some(match self {
            Lang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Js => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Py => tree_sitter_python::LANGUAGE.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Rb => tree_sitter_ruby::LANGUAGE.into(),
            // No maintained tree-sitter grammar exists for CoffeeScript;
            // it gets a line-based extractor like Markdown.
            Lang::Coffee | Lang::Md => return None,
        })
    }

    fn query_source(self) -> Option<&'static str> {
        Some(match self {
            Lang::Ts | Lang::Tsx => include_str!("../../../grammars/typescript/queries.scm"),
            Lang::Js => include_str!("../../../grammars/javascript/queries.scm"),
            Lang::Py => include_str!("../../../grammars/python/queries.scm"),
            Lang::Rust => include_str!("../../../grammars/rust/queries.scm"),
            Lang::Go => include_str!("../../../grammars/go/queries.scm"),
            Lang::Rb => include_str!("../../../grammars/ruby/queries.scm"),
            Lang::Coffee | Lang::Md => return None,
        })
    }

    pub fn query(self) -> Option<&'static Query> {
        static QUERIES: [OnceLock<Option<Query>>; 9] = [
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
        ];
        let ix = self as usize;
        QUERIES[ix]
            .get_or_init(|| {
                let language = self.language()?;
                let src = self.query_source()?;
                match Query::new(&language, src) {
                    Ok(q) => Some(q),
                    Err(e) => {
                        eprintln!("warning: query compile failed for {}: {e}", self.as_str());
                        None
                    }
                }
            })
            .as_ref()
    }
}

const MAX_PARSE_BYTES: usize = 1_048_576;
const MAX_ITEMS: usize = 400;
const MIN_NAME_LEN: usize = 3;

/// Extract the ParsedFile for one blob. Infallible: parse errors yield an
/// empty result rather than aborting an index run.
pub fn extract(lang: Lang, content: &[u8]) -> ParsedFile {
    if content.len() > MAX_PARSE_BYTES {
        return ParsedFile::default();
    }
    if lang == Lang::Md {
        return extract_markdown(content);
    }
    if lang == Lang::Coffee {
        return extract_coffee(content);
    }
    let (Some(language), Some(query)) = (lang.language(), lang.query()) else {
        return ParsedFile::default();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return ParsedFile::default();
    }
    let Some(tree) = parser.parse(content, None) else {
        return ParsedFile::default();
    };

    let mut imports: Vec<ImportRef> = Vec::new();
    let mut pending_names: Vec<String> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut def_names: BTreeSet<String> = BTreeSet::new();
    let mut calls: BTreeSet<String> = BTreeSet::new();
    let mut type_refs: BTreeSet<String> = BTreeSet::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), content);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = &query.capture_names()[cap.index as usize];
            let node = cap.node;
            let text = node.utf8_text(content).unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }
            match *name {
                "spec" | "spec.from" | "spec.reexport" => {
                    let spec = trim_quotes(&text);
                    if spec.is_empty() {
                        continue;
                    }
                    imports.push(ImportRef {
                        spec,
                        reexport: *name == "spec.reexport",
                        names: Vec::new(),
                    });
                }
                "spec.relative" => {
                    // Ruby require_relative: resolved against the file's own
                    // directory, marked so the resolver can tell it apart.
                    let spec = trim_quotes(&text);
                    if spec.is_empty() {
                        continue;
                    }
                    imports.push(ImportRef {
                        spec: format!("rel:{spec}"),
                        reexport: false,
                        names: Vec::new(),
                    });
                }
                "import.name" => pending_names.push(text),
                "use_decl" => {
                    // Rust: expand `use a::{b, c::d};` textually.
                    for (path, reexport) in expand_rust_use(&text) {
                        imports.push(ImportRef {
                            spec: path,
                            reexport,
                            names: Vec::new(),
                        });
                    }
                }
                "mod" => {
                    imports.push(ImportRef {
                        spec: format!("mod:{text}"),
                        reexport: false,
                        names: Vec::new(),
                    });
                }
                "call" => {
                    if text.len() >= MIN_NAME_LEN && calls.len() < MAX_ITEMS {
                        calls.insert(text);
                    }
                }
                "typeref" => {
                    if text.len() >= MIN_NAME_LEN && type_refs.len() < MAX_ITEMS {
                        type_refs.insert(text);
                    }
                }
                n if n.starts_with("def.") => {
                    if defs.len() >= MAX_ITEMS {
                        continue;
                    }
                    if n == "def.const" && !is_top_level_declarator(node) {
                        continue;
                    }
                    if def_names.insert(text.clone()) {
                        defs.push(SymbolDef {
                            name: text,
                            kind: n["def.".len()..].to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // Names imported by the file, attached to the last import lacking names.
    // Precise attachment does not matter downstream; the resolver treats the
    // union of imported names as symbol candidates.
    if !pending_names.is_empty() {
        if let Some(first) = imports.first_mut() {
            first.names = pending_names;
        } else {
            // Names with no module spec (should not happen); keep them as an
            // anonymous import so symbol resolution can still use them.
            imports.push(ImportRef {
                spec: String::new(),
                reexport: false,
                names: pending_names,
            });
        }
    }

    // A file does not call or reference itself through the graph.
    let calls: Vec<String> = calls
        .into_iter()
        .filter(|c| !def_names.contains(c))
        .collect();
    let type_refs: Vec<String> = type_refs
        .into_iter()
        .filter(|t| !def_names.contains(t))
        .collect();

    ParsedFile {
        imports: dedup_imports(imports),
        calls,
        type_refs,
        defs,
        doc_links: Vec::new(),
        doc_tokens: Vec::new(),
    }
}

fn dedup_imports(imports: Vec<ImportRef>) -> Vec<ImportRef> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for i in imports {
        if seen.insert((i.spec.clone(), i.reexport)) {
            out.push(i);
            if out.len() >= 200 {
                break;
            }
        }
    }
    out
}

fn trim_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

/// True when a variable_declarator sits at module top level (possibly under
/// an export statement). Local variables are not symbols.
fn is_top_level_declarator(node: tree_sitter::Node) -> bool {
    let Some(decl) = node.parent() else {
        return false;
    };
    let Some(stmt) = decl.parent() else {
        return false;
    };
    match stmt.kind() {
        "program" => true,
        "export_statement" => true,
        _ => stmt
            .parent()
            .map(|p| p.kind() == "program" || p.kind() == "export_statement")
            .unwrap_or(false),
    }
}

/// Expand a Rust use declaration's text into full paths.
/// "pub use crate::a::{b, c::d};" becomes [("crate::a::b", true), ("crate::a::c::d", true)].
pub fn expand_rust_use(decl: &str) -> Vec<(String, bool)> {
    let s = decl.trim().trim_end_matches(';').trim();
    let reexport = s.starts_with("pub");
    let s = s
        .trim_start_matches("pub(crate)")
        .trim_start_matches("pub(super)")
        .trim_start_matches("pub")
        .trim()
        .trim_start_matches("use")
        .trim();
    let mut out = Vec::new();
    expand_use_tree(s, "", &mut out);
    out.into_iter().map(|p| (p, reexport)).collect()
}

fn expand_use_tree(s: &str, prefix: &str, out: &mut Vec<String>) {
    let s = s.trim();
    if s.is_empty() || out.len() > 100 {
        return;
    }
    if let Some(brace) = s.find('{') {
        if !s.ends_with('}') {
            return;
        }
        let head = s[..brace].trim().trim_end_matches("::");
        let inner = &s[brace + 1..s.len() - 1];
        let new_prefix = join_path(prefix, head);
        for part in split_top_level(inner) {
            expand_use_tree(&part, &new_prefix, out);
        }
    } else {
        // Strip "as alias".
        let path = s.split(" as ").next().unwrap_or(s).trim();
        if path.is_empty() {
            return;
        }
        out.push(join_path(prefix, path));
    }
}

fn join_path(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_string()
    } else if tail.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{tail}")
    }
}

/// Split on commas not inside braces.
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

/// Line-based CoffeeScript extraction. CoffeeScript has no maintained
/// tree-sitter grammar (its indentation-sensitive syntax resists one), so
/// this follows the Markdown precedent: conservative regexes over lines,
/// favoring missed edges over invented ones.
fn extract_coffee(content: &[u8]) -> ParsedFile {
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    static EXPORT_FROM: OnceLock<Regex> = OnceLock::new();
    static REQUIRE: OnceLock<Regex> = OnceLock::new();
    static REQUIRE_NAMES: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static FUNC: OnceLock<Regex> = OnceLock::new();
    static CALL: OnceLock<Regex> = OnceLock::new();
    static NEW: OnceLock<Regex> = OnceLock::new();
    static IDENT: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORT.get_or_init(|| {
        Regex::new(r#"^\s*import\s+(?:(.+?)\s+from\s+)?["']([^"']+)["']"#).unwrap()
    });
    let export_from_re = EXPORT_FROM
        .get_or_init(|| Regex::new(r#"^\s*export\s+.*?\bfrom\s+["']([^"']+)["']"#).unwrap());
    let require_re = REQUIRE
        .get_or_init(|| Regex::new(r#"\brequire\s*\(?\s*["']([^"']+)["']"#).unwrap());
    let require_names_re = REQUIRE_NAMES.get_or_init(|| {
        Regex::new(r#"^\s*(?:\{([^}]+)\}|([A-Za-z_$][\w$]*))\s*=\s*require\b"#).unwrap()
    });
    let class_re = CLASS.get_or_init(|| {
        Regex::new(r"^\s*class\s+([A-Za-z_$][\w$.]*)(?:\s+extends\s+([A-Za-z_$][\w$.]*))?")
            .unwrap()
    });
    let func_re = FUNC.get_or_init(|| {
        Regex::new(r"^\s*([A-Za-z_$][\w$]*)\s*[:=]\s*(?:\([^)]*\)\s*)?[-=]>").unwrap()
    });
    let call_re = CALL.get_or_init(|| Regex::new(r"([A-Za-z_$][\w$]{2,})\s*\(").unwrap());
    let new_re = NEW.get_or_init(|| Regex::new(r"\bnew\s+([A-Za-z_$][\w$.]*)").unwrap());
    let ident_re = IDENT.get_or_init(|| Regex::new(r"[A-Za-z_$][\w$]*").unwrap());

    const CALL_STOPWORDS: &[&str] = &[
        "require", "return", "unless", "until", "while", "switch", "catch", "throw", "typeof",
        "super", "then", "when",
    ];

    let text = String::from_utf8_lossy(content);
    let mut imports: Vec<ImportRef> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut def_names: BTreeSet<String> = BTreeSet::new();
    let mut calls: BTreeSet<String> = BTreeSet::new();
    let mut type_refs: BTreeSet<String> = BTreeSet::new();
    let mut in_block_comment = false;

    let push_names = |clause: &str, names: &mut Vec<String>| {
        for m in ident_re.find_iter(clause) {
            let n = m.as_str();
            if n != "as" && n != "default" && n.len() >= MIN_NAME_LEN {
                names.push(n.to_string());
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim_start();
        if in_block_comment {
            if trimmed.contains("###") {
                in_block_comment = false;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("###") {
            // A one-line ### comment ### closes itself.
            if !rest.contains("###") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(cap) = import_re.captures(line) {
            if let Some(clause) = cap.get(1) {
                push_names(clause.as_str(), &mut names);
            }
            imports.push(ImportRef {
                spec: cap[2].to_string(),
                reexport: false,
                names: Vec::new(),
            });
            continue;
        }
        if let Some(cap) = export_from_re.captures(line) {
            imports.push(ImportRef {
                spec: cap[1].to_string(),
                reexport: true,
                names: Vec::new(),
            });
            continue;
        }
        if let Some(cap) = require_re.captures(line) {
            imports.push(ImportRef {
                spec: cap[1].to_string(),
                reexport: false,
                names: Vec::new(),
            });
            if let Some(nc) = require_names_re.captures(line) {
                if let Some(destructured) = nc.get(1) {
                    push_names(destructured.as_str(), &mut names);
                } else if let Some(single) = nc.get(2) {
                    push_names(single.as_str(), &mut names);
                }
            }
        }
        if let Some(cap) = class_re.captures(line) {
            let full = &cap[1];
            let name = full.rsplit('.').next().unwrap_or(full).to_string();
            if defs.len() < MAX_ITEMS && def_names.insert(name.clone()) {
                defs.push(SymbolDef {
                    name,
                    kind: "class".to_string(),
                });
            }
            if let Some(parent) = cap.get(2) {
                let p = parent.as_str();
                let p = p.rsplit('.').next().unwrap_or(p);
                if p.len() >= MIN_NAME_LEN && type_refs.len() < MAX_ITEMS {
                    type_refs.insert(p.to_string());
                }
            }
            continue;
        }
        if let Some(cap) = func_re.captures(line) {
            let name = cap[1].to_string();
            if name.len() >= MIN_NAME_LEN
                && defs.len() < MAX_ITEMS
                && def_names.insert(name.clone())
            {
                defs.push(SymbolDef {
                    name,
                    kind: "fn".to_string(),
                });
            }
        }
        for cap in call_re.captures_iter(line) {
            let name = &cap[1];
            if !CALL_STOPWORDS.contains(&name) && calls.len() < MAX_ITEMS {
                calls.insert(name.to_string());
            }
        }
        for cap in new_re.captures_iter(line) {
            let full = &cap[1];
            let name = full.rsplit('.').next().unwrap_or(full);
            if name.len() >= MIN_NAME_LEN && type_refs.len() < MAX_ITEMS {
                type_refs.insert(name.to_string());
            }
        }
    }

    if !names.is_empty() {
        if let Some(first) = imports.first_mut() {
            first.names = names;
        }
    }
    let calls: Vec<String> = calls
        .into_iter()
        .filter(|c| !def_names.contains(c))
        .collect();
    let type_refs: Vec<String> = type_refs
        .into_iter()
        .filter(|t| !def_names.contains(t))
        .collect();
    ParsedFile {
        imports: dedup_imports(imports),
        calls,
        type_refs,
        defs,
        doc_links: Vec::new(),
        doc_tokens: Vec::new(),
    }
}

fn extract_markdown(content: &[u8]) -> ParsedFile {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static CODE: OnceLock<Regex> = OnceLock::new();
    let link_re = LINK.get_or_init(|| Regex::new(r"\]\(([^()\s]+)\)").unwrap());
    let code_re = CODE.get_or_init(|| Regex::new(r"`([^`\n]{3,120})`").unwrap());
    let text = String::from_utf8_lossy(content);
    let mut links = BTreeSet::new();
    let mut tokens = BTreeSet::new();
    for cap in link_re.captures_iter(&text) {
        let target = cap[1].split('#').next().unwrap_or("");
        if target.is_empty()
            || target.contains("://")
            || target.starts_with("mailto:")
            || target.starts_with('#')
        {
            continue;
        }
        links.insert(target.to_string());
        if links.len() >= 200 {
            break;
        }
    }
    for cap in code_re.captures_iter(&text) {
        let tok = cap[1].trim().trim_end_matches("()").to_string();
        if tok.len() < MIN_NAME_LEN || tokens.len() >= 200 {
            continue;
        }
        let identifier_like = tok
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.');
        let path_like = tok.contains('/') || Lang::from_path(&tok).is_some();
        if path_like {
            links.insert(tok);
        } else if identifier_like
            && tok
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            // Keep the trailing segment of Foo::bar or a.b style tokens.
            let last = tok
                .rsplit([':', '.'])
                .next()
                .unwrap_or(&tok)
                .to_string();
            if last.len() >= MIN_NAME_LEN {
                tokens.insert(last);
            }
        }
    }
    ParsedFile {
        doc_links: links.into_iter().collect(),
        doc_tokens: tokens.into_iter().collect(),
        ..Default::default()
    }
}

/// Does a changed line look like an import line for this language?
/// Used for the mechanical-churn classifier.
pub fn is_import_line(lang: Option<Lang>, line: &str) -> bool {
    let l = line.trim_start();
    if l.is_empty() {
        return false;
    }
    match lang {
        Some(Lang::Ts | Lang::Tsx | Lang::Js) => {
            l.starts_with("import ")
                || l.starts_with("import{")
                || l.starts_with("} from ")
                || (l.starts_with("export ") && l.contains(" from "))
                || l.contains("require(")
                || (l.trim_end().ends_with(&[',', ';'][..]) && l.contains(" from "))
        }
        Some(Lang::Py) => l.starts_with("import ") || l.starts_with("from "),
        Some(Lang::Rust) => {
            l.starts_with("use ")
                || l.starts_with("pub use ")
                || l.starts_with("pub(crate) use ")
                || l.starts_with("extern crate ")
                || (l.starts_with("mod ") && l.trim_end().ends_with(';'))
        }
        Some(Lang::Go) => {
            l.starts_with("import ") || (l.starts_with('"') && l.trim_end().ends_with('"'))
        }
        Some(Lang::Rb) => {
            l.starts_with("require ")
                || l.starts_with("require(")
                || l.starts_with("require_relative ")
                || l.starts_with("require_relative(")
                || l.starts_with("load ")
                || l.starts_with("autoload ")
        }
        Some(Lang::Coffee) => {
            l.starts_with("import ")
                || (l.starts_with("export ") && l.contains(" from "))
                || l.contains("require(")
                || l.contains("require '")
                || l.contains("require \"")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_compile() {
        for lang in [
            Lang::Ts,
            Lang::Tsx,
            Lang::Js,
            Lang::Py,
            Lang::Rust,
            Lang::Go,
            Lang::Rb,
        ] {
            assert!(lang.query().is_some(), "query failed for {}", lang.as_str());
        }
    }

    #[test]
    fn ts_imports_and_defs() {
        let src = br#"
import { Frame } from "./frame";
import Codec from "../codec";
export * from "./reexported";
const helper = require("./helper");

export interface Encoder { frame(): Frame }
export function encode(f: Frame): Codec { return transform(f); }
const TOP = 1;
function inner() { const local = 2; }
"#;
        let p = extract(Lang::Ts, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"./frame"));
        assert!(specs.contains(&"../codec"));
        assert!(specs.contains(&"./reexported"));
        assert!(specs.contains(&"./helper"));
        assert!(p
            .imports
            .iter()
            .any(|i| i.reexport && i.spec == "./reexported"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"encode"));
        assert!(defs.contains(&"TOP"));
        assert!(!defs.contains(&"local"));
        assert!(p.calls.contains(&"transform".to_string()));
        assert!(p.type_refs.contains(&"Frame".to_string()));
    }

    #[test]
    fn rust_use_expansion() {
        let out = expand_rust_use("pub use crate::a::{b, c::d};");
        let paths: Vec<&str> = out.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["crate::a::b", "crate::a::c::d"]);
        assert!(out.iter().all(|(_, re)| *re));
        let plain = expand_rust_use("use super::codec::Codec as C;");
        assert_eq!(plain[0].0, "super::codec::Codec");
        assert!(!plain[0].1);
    }

    #[test]
    fn rust_extraction() {
        let src = br#"
use crate::frame::Frame;
mod codec;

pub struct Encoder { frame: Frame }
pub fn encode(f: &Frame) -> Vec<u8> { helper(f) }
"#;
        let p = extract(Lang::Rust, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"crate::frame::Frame"));
        assert!(specs.contains(&"mod:codec"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"encode"));
        assert!(p.calls.contains(&"helper".to_string()));
        assert!(p.type_refs.contains(&"Frame".to_string()));
    }

    #[test]
    fn python_extraction() {
        let src = br#"
import os
from .frame import Frame
from ..codec import encode_all

class Encoder:
    def encode(self, f):
        return transform(f)
"#;
        let p = extract(Lang::Py, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"os"));
        assert!(specs.contains(&".frame"));
        assert!(specs.contains(&"..codec"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"encode"));
        assert!(p.calls.contains(&"transform".to_string()));
    }

    #[test]
    fn go_extraction() {
        let src = br#"
package main

import (
    "fmt"
    "example.com/mod/internal/frame"
)

type Encoder struct{}

func Encode(f frame.Frame) { fmt.Println(process(f)) }
"#;
        let p = extract(Lang::Go, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"fmt"));
        assert!(specs.contains(&"example.com/mod/internal/frame"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"Encode"));
    }

    #[test]
    fn ruby_extraction() {
        let src = br#"
require "json"
require_relative "frame"
require_relative "../codec/base"

CODEC_VERSION = 2

module Codec
  class Encoder < Base
    def encode(f)
      transform(f)
    end

    def self.build
      new
    end
  end
end
"#;
        let p = extract(Lang::Rb, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"json"));
        assert!(specs.contains(&"rel:frame"));
        assert!(specs.contains(&"rel:../codec/base"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Codec"));
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"encode"));
        assert!(defs.contains(&"build"));
        assert!(defs.contains(&"CODEC_VERSION"));
        assert!(p.calls.contains(&"transform".to_string()));
        assert!(p.type_refs.contains(&"Base".to_string()));
    }

    #[test]
    fn coffee_extraction() {
        let src = br#"
import { Frame, mkFrame } from './frame'
import Codec from '../codec'
export { helper } from './helper'
{parse} = require './parser'
util = require("./util")

### block comment
encoder = -> ignored()
###
# line comment: fake = require './fake'

class Encoder extends BaseCodec
  encode: (f) ->
    transform(f)

toWire = (f) => serialize f

new Registry()
"#;
        let p = extract(Lang::Coffee, src);
        let specs: Vec<&str> = p.imports.iter().map(|i| i.spec.as_str()).collect();
        assert!(specs.contains(&"./frame"));
        assert!(specs.contains(&"../codec"));
        assert!(specs.contains(&"./helper"));
        assert!(specs.contains(&"./parser"));
        assert!(specs.contains(&"./util"));
        assert!(!specs.contains(&"./fake"));
        assert!(p.imports.iter().any(|i| i.reexport && i.spec == "./helper"));
        let names: Vec<&str> = p
            .imports
            .iter()
            .flat_map(|i| i.names.iter().map(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"Frame"));
        assert!(names.contains(&"mkFrame"));
        assert!(names.contains(&"Codec"));
        assert!(names.contains(&"parse"));
        let defs: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(defs.contains(&"Encoder"));
        assert!(defs.contains(&"encode"));
        assert!(defs.contains(&"toWire"));
        assert!(!defs.contains(&"encoder"), "block comment not skipped");
        assert!(p.calls.contains(&"transform".to_string()));
        assert!(p.type_refs.contains(&"BaseCodec".to_string()));
        assert!(p.type_refs.contains(&"Registry".to_string()));
    }

    #[test]
    fn markdown_extraction() {
        let src = br#"
# Auth

See [the auth module](../src/auth.ts) and [docs](https://example.com/x).
Call `verify_token()` before `src/session.ts` logic.
"#;
        let p = extract(Lang::Md, src);
        assert!(p.doc_links.contains(&"../src/auth.ts".to_string()));
        assert!(p.doc_links.contains(&"src/session.ts".to_string()));
        assert!(p.doc_tokens.contains(&"verify_token".to_string()));
        assert!(!p.doc_links.iter().any(|l| l.contains("example.com")));
    }

    #[test]
    fn import_line_detection() {
        assert!(is_import_line(Some(Lang::Ts), "import { x } from './y';"));
        assert!(is_import_line(Some(Lang::Rust), "use crate::foo::Bar;"));
        assert!(is_import_line(Some(Lang::Py), "from x import y"));
        assert!(!is_import_line(Some(Lang::Ts), "const x = 1;"));
    }
}
