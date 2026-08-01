//! Domain types and metric math for palimpsest. No I/O lives here.

pub mod metrics;
pub mod parsed;
pub mod time;

use serde::{Deserialize, Serialize};

/// Dense internal id for a file identity that survives renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub i64);

/// Dense internal id for a commit, ordered oldest to newest along the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CommitId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Import,
    Call,
    TypeRef,
    DocRef,
    Reexport,
}

impl EdgeKind {
    pub fn to_i64(self) -> i64 {
        match self {
            EdgeKind::Import => 0,
            EdgeKind::Call => 1,
            EdgeKind::TypeRef => 2,
            EdgeKind::DocRef => 3,
            EdgeKind::Reexport => 4,
        }
    }
    pub fn from_i64(v: i64) -> Option<Self> {
        Some(match v {
            0 => EdgeKind::Import,
            1 => EdgeKind::Call,
            2 => EdgeKind::TypeRef,
            3 => EdgeKind::DocRef,
            4 => EdgeKind::Reexport,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Import => "import",
            EdgeKind::Call => "call",
            EdgeKind::TypeRef => "typeref",
            EdgeKind::DocRef => "docref",
            EdgeKind::Reexport => "reexport",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "import" => EdgeKind::Import,
            "call" => EdgeKind::Call,
            "typeref" => EdgeKind::TypeRef,
            "docref" => EdgeKind::DocRef,
            "reexport" => EdgeKind::Reexport,
            _ => return None,
        })
    }
}

/// How an edge target was resolved. Lower is more exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    PathExact,
    SymbolName,
    Heuristic,
}

impl Resolution {
    pub fn to_i64(self) -> i64 {
        match self {
            Resolution::PathExact => 0,
            Resolution::SymbolName => 1,
            Resolution::Heuristic => 2,
        }
    }
    pub fn from_i64(v: i64) -> Option<Self> {
        Some(match v {
            0 => Resolution::PathExact,
            1 => Resolution::SymbolName,
            2 => Resolution::Heuristic,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::PathExact => "path-exact",
            Resolution::SymbolName => "symbol-name",
            Resolution::Heuristic => "heuristic",
        }
    }
}

/// Kind of change a commit made to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    pub fn to_i64(self) -> i64 {
        match self {
            ChangeKind::Added => 0,
            ChangeKind::Modified => 1,
            ChangeKind::Deleted => 2,
            ChangeKind::Renamed => 3,
        }
    }
    pub fn from_i64(v: i64) -> Option<Self> {
        Some(match v {
            0 => ChangeKind::Added,
            1 => ChangeKind::Modified,
            2 => ChangeKind::Deleted,
            3 => ChangeKind::Renamed,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
        }
    }
}

/// Bitflags recording why a commit was excluded from co-change metrics.
pub mod excluded {
    pub const TOO_LARGE: i64 = 1;
    pub const MECHANICAL: i64 = 2;
    pub const VENDORED: i64 = 4;
    pub const MERGE: i64 = 8;
}

/// One piece of evidence tying two files together, with provenance.
/// Never collapse these into a single score: the reader is deciding what to
/// open, and "severed in 2023, still co-changes" means something a bare
/// number does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Evidence {
    Structural {
        kind: EdgeKind,
        alive: bool,
        resolution: Resolution,
        direction: Direction,
    },
    Ghost {
        kind: EdgeKind,
        severed_at: String,
        severed_date: String,
        lifetime_days: i64,
        cochanges_since: i64,
        confidence_since: f64,
        severing_subject: String,
        direction: Direction,
    },
    Cochange {
        n: i64,
        support: f64,
        confidence: f64,
        lift: f64,
        last: String,
    },
    Transitive {
        via: Vec<String>,
        union_dist: u8,
        head_dist: Option<u8>,
    },
    DocDrift {
        doc: String,
        commits_behind: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Out,
    In,
}

/// A ranked relationship between the query file and another file.
#[derive(Debug, Clone, Serialize)]
pub struct Relation {
    pub path: String,
    #[serde(skip_serializing)]
    pub file_id: FileId,
    pub rank: f64,
    pub evidence: Vec<Evidence>,
}
