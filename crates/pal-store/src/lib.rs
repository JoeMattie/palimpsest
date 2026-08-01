//! SQLite persistence for palimpsest: schema, migrations, and typed queries.
//! One writer at a time; WAL mode so readers never block on the indexer.

mod schema;

use pal_core::parsed::ParsedFile;
use pal_core::{ChangeKind, CommitId, EdgeKind, FileId, Resolution};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub use schema::SCHEMA_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database not found at {0}; run `pal index` first")]
    Missing(String),
    #[error("schema version mismatch: db has {found}, this build expects {expected}; re-run `pal index`")]
    SchemaMismatch { found: i64, expected: i64 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Codec(#[from] postcard::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    pub conn: Connection,
}

#[derive(Debug, Clone)]
pub struct CommitRow {
    pub id: CommitId,
    pub oid: Vec<u8>,
    pub author_time: i64,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub n_files: i64,
    pub weight: f64,
    pub excluded: i64,
    pub is_merge: bool,
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: FileId,
    pub current_path: Option<String>,
    pub lang: Option<String>,
    pub born_commit: CommitId,
    pub died_commit: Option<CommitId>,
    pub is_doc: bool,
}

#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: i64,
    pub src: FileId,
    pub dst: FileId,
    pub kind: EdgeKind,
    pub resolution: Resolution,
}

#[derive(Debug, Clone)]
pub struct IntervalRow {
    pub edge_id: i64,
    pub born: CommitId,
    pub died: Option<CommitId>,
}

#[derive(Debug, Clone)]
pub struct CochangeRow {
    pub a: FileId,
    pub b: FileId,
    pub n: i64,
    pub w_support: f64,
    pub w_decayed: f64,
    pub conf_ab: f64,
    pub conf_ba: f64,
    pub lift: f64,
    pub first_commit: CommitId,
    pub last_commit: CommitId,
}

#[derive(Debug, Clone)]
pub struct GhostRow {
    pub edge_id: i64,
    pub severed_commit: CommitId,
    pub lifetime_days: i64,
    pub cochanges_since: i64,
    pub conf_since: f64,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct PathIntervalRow {
    pub file_id: FileId,
    pub path: String,
    pub from_commit: CommitId,
    pub to_commit: Option<CommitId>,
    pub confidence: f64,
}

pub fn short_oid(oid: &[u8]) -> String {
    oid.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

pub fn full_oid(oid: &[u8]) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect()
}

impl Store {
    /// Open, creating and migrating if absent.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(schema::INIT_SQL)?;
        let s = Store { conn };
        if s.meta_get("schema_version")?.is_none() {
            s.meta_set("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        s.check_version()?;
        Ok(s)
    }

    /// Open an existing database; error if missing or wrong version.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref();
        if !p.exists() {
            return Err(StoreError::Missing(p.display().to_string()));
        }
        let conn = Connection::open(p)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let s = Store { conn };
        s.check_version()?;
        Ok(s)
    }

    /// In-memory database for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::INIT_SQL)?;
        let s = Store { conn };
        s.meta_set("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(s)
    }

    fn check_version(&self) -> Result<()> {
        let found: i64 = self
            .meta_get("schema_version")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if found != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch {
                found,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    // ---- meta ----

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- transactions (single writer, explicit) ----

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit_tx(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    // ---- writer API: commits and touches ----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_commit(
        &self,
        oid: &[u8],
        parent_oid: Option<&[u8]>,
        author_time: i64,
        author: &str,
        subject: &str,
        body: &str,
        n_files: i64,
        weight: f64,
        excluded: i64,
        is_merge: bool,
    ) -> Result<CommitId> {
        self.conn.execute(
            "INSERT INTO commits(oid,parent_oid,author_time,author,subject,body,n_files,weight,excluded,is_merge)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![oid, parent_oid, author_time, author, subject, body, n_files, weight, excluded, is_merge as i64],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO commit_fts(rowid,subject,body) VALUES(?1,?2,?3)",
            params![id, subject, body],
        )?;
        Ok(CommitId(id))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_touch(
        &self,
        commit: CommitId,
        file: FileId,
        change: ChangeKind,
        blob_oid: Option<&[u8]>,
        lines_added: Option<i64>,
        lines_removed: Option<i64>,
        import_only: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO touches(commit_id,file_id,change,blob_oid,lines_added,lines_removed,import_only)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                commit.0,
                file.0,
                change.to_i64(),
                blob_oid,
                lines_added,
                lines_removed,
                import_only as i64
            ],
        )?;
        Ok(())
    }

    // ---- writer API: files and identity ----

    pub fn insert_file(
        &self,
        path: &str,
        lang: Option<&str>,
        born: CommitId,
        is_doc: bool,
    ) -> Result<FileId> {
        self.conn.execute(
            "INSERT INTO files(current_path,lang,born_commit,died_commit,is_doc)
             VALUES(?1,?2,?3,NULL,?4)",
            params![path, lang, born.0, is_doc as i64],
        )?;
        let id = FileId(self.conn.last_insert_rowid());
        self.conn.execute(
            "INSERT INTO file_paths(file_id,path,from_commit,to_commit,confidence)
             VALUES(?1,?2,?3,NULL,1.0)",
            params![id.0, path, born.0],
        )?;
        Ok(id)
    }

    /// Detach current_path (used before re-pointing paths within a commit so
    /// the unique index never sees a transient collision).
    pub fn file_clear_path(&self, file: FileId) -> Result<()> {
        self.conn
            .execute("UPDATE files SET current_path=NULL WHERE id=?1", [file.0])?;
        Ok(())
    }

    pub fn file_set_path(&self, file: FileId, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET current_path=?2 WHERE id=?1",
            params![file.0, path],
        )?;
        Ok(())
    }

    pub fn file_record_rename(
        &self,
        file: FileId,
        new_path: &str,
        at: CommitId,
        confidence: f64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE file_paths SET to_commit=?2 WHERE file_id=?1 AND to_commit IS NULL",
            params![file.0, at.0],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO file_paths(file_id,path,from_commit,to_commit,confidence)
             VALUES(?1,?2,?3,NULL,?4)",
            params![file.0, new_path, at.0, confidence],
        )?;
        Ok(())
    }

    pub fn file_set_died(&self, file: FileId, at: CommitId) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET died_commit=?2, current_path=NULL WHERE id=?1",
            params![file.0, at.0],
        )?;
        self.conn.execute(
            "UPDATE file_paths SET to_commit=?2 WHERE file_id=?1 AND to_commit IS NULL",
            params![file.0, at.0],
        )?;
        Ok(())
    }

    pub fn file_resurrect(&self, file: FileId, path: &str, at: CommitId) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET died_commit=NULL, current_path=?2 WHERE id=?1",
            params![file.0, path],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO file_paths(file_id,path,from_commit,to_commit,confidence)
             VALUES(?1,?2,?3,NULL,1.0)",
            params![file.0, path, at.0],
        )?;
        Ok(())
    }

    pub fn file_set_lang(&self, file: FileId, lang: Option<&str>, is_doc: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET lang=?2, is_doc=?3 WHERE id=?1",
            params![file.0, lang, is_doc as i64],
        )?;
        Ok(())
    }

    // ---- writer API: parse cache ----

    pub fn blob_parse_get(
        &self,
        blob_oid: &[u8],
        parser_version: i64,
    ) -> Result<Option<ParsedFile>> {
        let row: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT parsed FROM blob_parse WHERE blob_oid=?1 AND parser_version=?2",
                params![blob_oid, parser_version],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(bytes) => Ok(Some(postcard::from_bytes(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn blob_parse_put(
        &self,
        blob_oid: &[u8],
        lang: &str,
        parsed: &ParsedFile,
        parser_version: i64,
    ) -> Result<()> {
        let bytes = postcard::to_allocvec(parsed)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO blob_parse(blob_oid,lang,parsed,parser_version)
             VALUES(?1,?2,?3,?4)",
            params![blob_oid, lang, bytes, parser_version],
        )?;
        Ok(())
    }

    // ---- writer API: symbols ----

    pub fn symbol_open(&self, name: &str, file: FileId, kind: &str, at: CommitId) -> Result<()> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM symbols WHERE name=?1 AND file_id=?2 AND last_commit IS NULL",
                params![name, file.0],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_none() {
            self.conn.execute(
                "INSERT INTO symbols(name,file_id,kind,first_commit,last_commit)
                 VALUES(?1,?2,?3,?4,NULL)",
                params![name, file.0, kind, at.0],
            )?;
        }
        Ok(())
    }

    pub fn symbol_close(&self, name: &str, file: FileId, at: CommitId) -> Result<()> {
        self.conn.execute(
            "UPDATE symbols SET last_commit=?3 WHERE name=?1 AND file_id=?2 AND last_commit IS NULL",
            params![name, file.0, at.0],
        )?;
        Ok(())
    }

    pub fn symbols_close_all_for_file(&self, file: FileId, at: CommitId) -> Result<()> {
        self.conn.execute(
            "UPDATE symbols SET last_commit=?2 WHERE file_id=?1 AND last_commit IS NULL",
            params![file.0, at.0],
        )?;
        Ok(())
    }

    // ---- writer API: edges and intervals ----

    pub fn edge_get_or_create(
        &self,
        src: FileId,
        dst: FileId,
        kind: EdgeKind,
        resolution: Resolution,
    ) -> Result<i64> {
        let existing: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT id, resolution FROM edges WHERE src_file=?1 AND dst_file=?2 AND kind=?3",
                params![src.0, dst.0, kind.to_i64()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, res)) = existing {
            if resolution.to_i64() < res {
                self.conn.execute(
                    "UPDATE edges SET resolution=?2 WHERE id=?1",
                    params![id, resolution.to_i64()],
                )?;
            }
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO edges(src_file,dst_file,kind,resolution) VALUES(?1,?2,?3,?4)",
            params![src.0, dst.0, kind.to_i64(), resolution.to_i64()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn interval_open(&self, edge_id: i64, born: CommitId) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO edge_intervals(edge_id,born_commit,died_commit)
             VALUES(?1,?2,NULL)",
            params![edge_id, born.0],
        )?;
        Ok(())
    }

    pub fn interval_close(&self, edge_id: i64, died: CommitId) -> Result<()> {
        self.conn.execute(
            "UPDATE edge_intervals SET died_commit=?2 WHERE edge_id=?1 AND died_commit IS NULL",
            params![edge_id, died.0],
        )?;
        Ok(())
    }

    // ---- writer API: analysis products ----

    pub fn cochange_clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM cochange", [])?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cochange_insert(&self, row: &CochangeRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cochange(a,b,n,w_support,w_decayed,conf_ab,conf_ba,lift,first_commit,last_commit)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                row.a.0,
                row.b.0,
                row.n,
                row.w_support,
                row.w_decayed,
                row.conf_ab,
                row.conf_ba,
                row.lift,
                row.first_commit.0,
                row.last_commit.0
            ],
        )?;
        Ok(())
    }

    pub fn ghosts_clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM ghosts", [])?;
        Ok(())
    }

    pub fn ghost_insert(&self, row: &GhostRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ghosts(edge_id,severed_commit,lifetime_days,cochanges_since,conf_since,score)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                row.edge_id,
                row.severed_commit.0,
                row.lifetime_days,
                row.cochanges_since,
                row.conf_since,
                row.score
            ],
        )?;
        Ok(())
    }

    // ---- reader API ----

    pub fn commit_by_id(&self, id: CommitId) -> Result<Option<CommitRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,oid,author_time,author,subject,n_files,weight,excluded,is_merge
                 FROM commits WHERE id=?1",
                [id.0],
                |r| {
                    Ok(CommitRow {
                        id: CommitId(r.get(0)?),
                        oid: r.get(1)?,
                        author_time: r.get(2)?,
                        author: r.get(3)?,
                        subject: r.get(4)?,
                        n_files: r.get(5)?,
                        weight: r.get(6)?,
                        excluded: r.get(7)?,
                        is_merge: r.get::<_, i64>(8)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn all_commits(&self) -> Result<Vec<CommitRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,oid,author_time,author,subject,n_files,weight,excluded,is_merge
             FROM commits ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CommitRow {
                    id: CommitId(r.get(0)?),
                    oid: r.get(1)?,
                    author_time: r.get(2)?,
                    author: r.get(3)?,
                    subject: r.get(4)?,
                    n_files: r.get(5)?,
                    weight: r.get(6)?,
                    excluded: r.get(7)?,
                    is_merge: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn file_by_id(&self, id: FileId) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,current_path,lang,born_commit,died_commit,is_doc FROM files WHERE id=?1",
                [id.0],
                |r| {
                    Ok(FileRow {
                        id: FileId(r.get(0)?),
                        current_path: r.get(1)?,
                        lang: r.get(2)?,
                        born_commit: CommitId(r.get(3)?),
                        died_commit: r.get::<_, Option<i64>>(4)?.map(CommitId),
                        is_doc: r.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// Resolve a repo-relative path to a file id, trying current paths first
    /// and then the historical rename chain (most recent occupant wins).
    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        let by_current: Option<i64> = self
            .conn
            .query_row("SELECT id FROM files WHERE current_path=?1", [path], |r| {
                r.get(0)
            })
            .optional()?;
        let id = match by_current {
            Some(id) => Some(id),
            None => self
                .conn
                .query_row(
                    "SELECT file_id FROM file_paths WHERE path=?1 ORDER BY from_commit DESC LIMIT 1",
                    [path],
                    |r| r.get(0),
                )
                .optional()?,
        };
        match id {
            Some(id) => self.file_by_id(FileId(id)),
            None => Ok(None),
        }
    }

    /// Best display path for a file: current path, or its last known path
    /// with a "(deleted)" marker left to the caller.
    pub fn display_path(&self, id: FileId) -> Result<String> {
        if let Some(f) = self.file_by_id(id)? {
            if let Some(p) = f.current_path {
                return Ok(p);
            }
        }
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM file_paths WHERE file_id=?1 ORDER BY from_commit DESC LIMIT 1",
                [id.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(last.unwrap_or_else(|| format!("<file {}>", id.0)))
    }

    pub fn file_paths_for(&self, id: FileId) -> Result<Vec<PathIntervalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id,path,from_commit,to_commit,confidence
             FROM file_paths WHERE file_id=?1 ORDER BY from_commit",
        )?;
        let rows = stmt
            .query_map([id.0], |r| {
                Ok(PathIntervalRow {
                    file_id: FileId(r.get(0)?),
                    path: r.get(1)?,
                    from_commit: CommitId(r.get(2)?),
                    to_commit: r.get::<_, Option<i64>>(3)?.map(CommitId),
                    confidence: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_files(&self) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,current_path,lang,born_commit,died_commit,is_doc FROM files ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FileRow {
                    id: FileId(r.get(0)?),
                    current_path: r.get(1)?,
                    lang: r.get(2)?,
                    born_commit: CommitId(r.get(3)?),
                    died_commit: r.get::<_, Option<i64>>(4)?.map(CommitId),
                    is_doc: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// (commit_id, file_id, import_only, weighted) for all touches, ordered
    /// by commit. The analyzer streams this once.
    pub fn all_touches(&self) -> Result<Vec<(CommitId, FileId, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT commit_id,file_id,import_only FROM touches ORDER BY commit_id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    CommitId(r.get(0)?),
                    FileId(r.get(1)?),
                    r.get::<_, i64>(2)? != 0,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn touches_for_file(&self, id: FileId) -> Result<Vec<(CommitId, ChangeKind)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT commit_id,change FROM touches WHERE file_id=?1 ORDER BY commit_id")?;
        let rows = stmt
            .query_map([id.0], |r| {
                Ok((
                    CommitId(r.get(0)?),
                    ChangeKind::from_i64(r.get(1)?).unwrap_or(ChangeKind::Modified),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_edges(&self) -> Result<Vec<EdgeRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id,src_file,dst_file,kind,resolution FROM edges")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EdgeRow {
                    id: r.get(0)?,
                    src: FileId(r.get(1)?),
                    dst: FileId(r.get(2)?),
                    kind: EdgeKind::from_i64(r.get(3)?).unwrap_or(EdgeKind::Import),
                    resolution: Resolution::from_i64(r.get(4)?).unwrap_or(Resolution::Heuristic),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn edge_by_id(&self, id: i64) -> Result<Option<EdgeRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,src_file,dst_file,kind,resolution FROM edges WHERE id=?1",
                [id],
                |r| {
                    Ok(EdgeRow {
                        id: r.get(0)?,
                        src: FileId(r.get(1)?),
                        dst: FileId(r.get(2)?),
                        kind: EdgeKind::from_i64(r.get(3)?).unwrap_or(EdgeKind::Import),
                        resolution: Resolution::from_i64(r.get(4)?)
                            .unwrap_or(Resolution::Heuristic),
                    })
                },
            )
            .optional()?)
    }

    pub fn intervals_for_edge(&self, edge_id: i64) -> Result<Vec<IntervalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id,born_commit,died_commit FROM edge_intervals
             WHERE edge_id=?1 ORDER BY born_commit",
        )?;
        let rows = stmt
            .query_map([edge_id], |r| {
                Ok(IntervalRow {
                    edge_id: r.get(0)?,
                    born: CommitId(r.get(1)?),
                    died: r.get::<_, Option<i64>>(2)?.map(CommitId),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_intervals(&self) -> Result<Vec<IntervalRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id,born_commit,died_commit FROM edge_intervals ORDER BY edge_id,born_commit",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(IntervalRow {
                    edge_id: r.get(0)?,
                    born: CommitId(r.get(1)?),
                    died: r.get::<_, Option<i64>>(2)?.map(CommitId),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn cochange_for_file(&self, id: FileId) -> Result<Vec<CochangeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT a,b,n,w_support,w_decayed,conf_ab,conf_ba,lift,first_commit,last_commit
             FROM cochange WHERE a=?1 OR b=?1 ORDER BY w_decayed DESC",
        )?;
        let rows = stmt
            .query_map([id.0], |r| {
                Ok(CochangeRow {
                    a: FileId(r.get(0)?),
                    b: FileId(r.get(1)?),
                    n: r.get(2)?,
                    w_support: r.get(3)?,
                    w_decayed: r.get(4)?,
                    conf_ab: r.get(5)?,
                    conf_ba: r.get(6)?,
                    lift: r.get(7)?,
                    first_commit: CommitId(r.get(8)?),
                    last_commit: CommitId(r.get(9)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn cochange_pair(&self, a: FileId, b: FileId) -> Result<Option<CochangeRow>> {
        let (lo, hi) = if a.0 <= b.0 { (a, b) } else { (b, a) };
        Ok(self
            .conn
            .query_row(
                "SELECT a,b,n,w_support,w_decayed,conf_ab,conf_ba,lift,first_commit,last_commit
                 FROM cochange WHERE a=?1 AND b=?2",
                [lo.0, hi.0],
                |r| {
                    Ok(CochangeRow {
                        a: FileId(r.get(0)?),
                        b: FileId(r.get(1)?),
                        n: r.get(2)?,
                        w_support: r.get(3)?,
                        w_decayed: r.get(4)?,
                        conf_ab: r.get(5)?,
                        conf_ba: r.get(6)?,
                        lift: r.get(7)?,
                        first_commit: CommitId(r.get(8)?),
                        last_commit: CommitId(r.get(9)?),
                    })
                },
            )
            .optional()?)
    }

    pub fn all_cochange(&self) -> Result<Vec<CochangeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT a,b,n,w_support,w_decayed,conf_ab,conf_ba,lift,first_commit,last_commit
             FROM cochange",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CochangeRow {
                    a: FileId(r.get(0)?),
                    b: FileId(r.get(1)?),
                    n: r.get(2)?,
                    w_support: r.get(3)?,
                    w_decayed: r.get(4)?,
                    conf_ab: r.get(5)?,
                    conf_ba: r.get(6)?,
                    lift: r.get(7)?,
                    first_commit: CommitId(r.get(8)?),
                    last_commit: CommitId(r.get(9)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_ghosts(&self) -> Result<Vec<GhostRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id,severed_commit,lifetime_days,cochanges_since,conf_since,score
             FROM ghosts ORDER BY score DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GhostRow {
                    edge_id: r.get(0)?,
                    severed_commit: CommitId(r.get(1)?),
                    lifetime_days: r.get(2)?,
                    cochanges_since: r.get(3)?,
                    conf_since: r.get(4)?,
                    score: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn fts_search(&self, query: &str, limit: i64) -> Result<Vec<(CommitId, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.subject, c.author FROM commit_fts f
             JOIN commits c ON c.id = f.rowid
             WHERE commit_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![query, limit], |r| {
                Ok((
                    CommitId(r.get(0)?),
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count(&self, sql: &str) -> Result<i64> {
        Ok(self.conn.query_row(sql, [], |r| r.get(0))?)
    }
}
