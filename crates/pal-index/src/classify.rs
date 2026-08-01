//! Commit classification: size, mechanical churn, and vendored paths.
//! This determines signal quality; see plan section 3.4. Every decision is
//! recorded as flags so a consumer can re-derive with different thresholds.

use crate::langs::{is_import_line, Lang};
use pal_core::excluded;
use pal_core::metrics::Params;
use regex::Regex;
use std::sync::OnceLock;

/// Default path prefixes and file names excluded from indexing entirely.
pub fn is_excluded_path(path: &str) -> bool {
    const DIR_MARKERS: &[&str] = &[
        ".pal/",
        "node_modules/",
        "vendor/",
        "target/",
        "dist/",
        "build/",
        ".yarn/",
        "third_party/",
        "__pycache__/",
        ".venv/",
        "venv/",
    ];
    const FILES: &[&str] = &[
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "poetry.lock",
        "uv.lock",
        "go.sum",
        "composer.lock",
        "Gemfile.lock",
    ];
    const SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map", ".snap", ".lock"];

    for marker in DIR_MARKERS {
        if path.starts_with(marker) || path.contains(&format!("/{marker}")) {
            return true;
        }
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    if FILES.contains(&base) {
        return true;
    }
    SUFFIXES.iter().any(|s| path.ends_with(s))
}

fn mechanical_subject_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(chore|style|fmt|lint)\b|prettier|rustfmt|gofmt|reformat|^bump\b|rename.*import|import.*reorder|alias migration",
        )
        .unwrap()
    })
}

/// Fraction of changed lines that are import lines, per file.
/// Blank and brace-only lines are ignored so a moved import block does not
/// get diluted by its surrounding whitespace.
pub fn import_only_fraction(lang: Option<Lang>, changed_lines: &[String]) -> f64 {
    let mut considered = 0usize;
    let mut import = 0usize;
    for line in changed_lines {
        let t = line.trim();
        if t.is_empty() || t == "}" || t == "};" || t == ")" || t == ");" || t == "{" {
            continue;
        }
        considered += 1;
        if is_import_line(lang, line) {
            import += 1;
        }
    }
    if considered == 0 {
        0.0
    } else {
        import as f64 / considered as f64
    }
}

pub struct CommitClass {
    pub excluded_flags: i64,
    pub weight: f64,
}

/// Classify a commit given its per-file import_only flags and metadata.
/// `import_only` holds one bool per surviving (non-vendored) file change.
pub fn classify_commit(
    params: &Params,
    subject: &str,
    n_files: usize,
    import_only: &[bool],
    is_merge: bool,
    exclude_merges: bool,
) -> CommitClass {
    let mut flags = 0i64;
    if n_files > params.max_commit_files {
        flags |= excluded::TOO_LARGE;
    }
    let import_only_count = import_only.iter().filter(|b| **b).count();
    let mostly_imports = n_files > 5 && import_only_count as f64 / n_files.max(1) as f64 > 0.9;
    if mostly_imports || (mechanical_subject_re().is_match(subject) && n_files > 5) {
        flags |= excluded::MECHANICAL;
    }
    // In a first-parent walk the merge commit carries the squashed content
    // of the branch, so MERGE alone does not zero the weight; the flag is
    // recorded either way.
    if is_merge {
        flags |= excluded::MERGE;
    }
    let hard_excluded = flags & excluded::TOO_LARGE != 0
        || flags & excluded::MECHANICAL != 0
        || (exclude_merges && is_merge);
    let weight = pal_core::metrics::commit_weight(n_files, hard_excluded);
    CommitClass {
        excluded_flags: flags,
        weight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_paths() {
        assert!(is_excluded_path("node_modules/react/index.js"));
        assert!(is_excluded_path("pkg/node_modules/x.js"));
        assert!(is_excluded_path("Cargo.lock"));
        assert!(is_excluded_path("web/dist/app.min.js"));
        assert!(!is_excluded_path("src/main.rs"));
        assert!(!is_excluded_path("docs/guide.md"));
    }

    #[test]
    fn lint_storm_is_mechanical() {
        let p = Params::default();
        let import_only: Vec<bool> = vec![true; 40];
        let c = classify_commit(&p, "chore: reorder imports", 40, &import_only, false, false);
        assert!(c.excluded_flags & excluded::MECHANICAL != 0);
        assert_eq!(c.weight, 0.0);
    }

    #[test]
    fn big_commit_excluded() {
        let p = Params::default();
        let flags: Vec<bool> = vec![false; 400];
        let c = classify_commit(&p, "apply lint rule", 400, &flags, false, false);
        assert!(c.excluded_flags & excluded::TOO_LARGE != 0);
        assert_eq!(c.weight, 0.0);
    }

    #[test]
    fn normal_commit_weighted() {
        let p = Params::default();
        let c = classify_commit(&p, "fix encoder frame bug", 4, &[false; 4], false, false);
        assert_eq!(c.excluded_flags, 0);
        assert_eq!(c.weight, 0.25);
    }
}
