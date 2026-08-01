//! The parse result cached per blob oid. This is what tree-sitter extraction
//! produces and what edge resolution consumes. It must stay stable across
//! runs; bump PARSER_VERSION whenever its shape or the extraction queries
//! change so stale cache rows are invalidated.

use serde::{Deserialize, Serialize};

/// Bump on any change to extraction queries or this struct's encoding.
pub const PARSER_VERSION: i64 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParsedFile {
    pub imports: Vec<ImportRef>,
    /// Names called in call position that are not defined in this file.
    pub calls: Vec<String>,
    /// Names referenced in type position that are not defined in this file.
    pub type_refs: Vec<String>,
    pub defs: Vec<SymbolDef>,
    /// Markdown only: link targets that look like repo paths, and inline
    /// code tokens that may name symbols.
    pub doc_links: Vec<String>,
    pub doc_tokens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportRef {
    /// The raw module specifier as written: "./foo", "crate::bar", "pkg/util".
    pub spec: String,
    /// True for re-export forms such as `export * from "./x"` or `pub use`.
    pub reexport: bool,
    /// Names imported from the specifier, when the syntax exposes them.
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolDef {
    pub name: String,
    /// Free-form kind string from the grammar: "fn", "struct", "class", ...
    pub kind: String,
}
