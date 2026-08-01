pub const SCHEMA_VERSION: i64 = 1;

pub const INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS commits (
    id             INTEGER PRIMARY KEY,
    oid            BLOB NOT NULL UNIQUE,
    parent_oid     BLOB,
    author_time    INTEGER NOT NULL,
    author         TEXT,
    subject        TEXT,
    body           TEXT,
    n_files        INTEGER NOT NULL,
    weight         REAL NOT NULL,
    excluded       INTEGER NOT NULL,
    is_merge       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS commits_time ON commits(author_time);

CREATE TABLE IF NOT EXISTS files (
    id             INTEGER PRIMARY KEY,
    current_path   TEXT,
    lang           TEXT,
    born_commit    INTEGER NOT NULL,
    died_commit    INTEGER,
    is_doc         INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS files_path ON files(current_path)
    WHERE current_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS file_paths (
    file_id        INTEGER NOT NULL,
    path           TEXT NOT NULL,
    from_commit    INTEGER NOT NULL,
    to_commit      INTEGER,
    confidence     REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (file_id, from_commit)
);
CREATE INDEX IF NOT EXISTS file_paths_path ON file_paths(path);

CREATE TABLE IF NOT EXISTS touches (
    commit_id      INTEGER NOT NULL,
    file_id        INTEGER NOT NULL,
    change         INTEGER NOT NULL,
    blob_oid       BLOB,
    lines_added    INTEGER,
    lines_removed  INTEGER,
    import_only    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (commit_id, file_id)
);
CREATE INDEX IF NOT EXISTS touches_file ON touches(file_id);

CREATE TABLE IF NOT EXISTS blob_parse (
    blob_oid       BLOB PRIMARY KEY,
    lang           TEXT NOT NULL,
    parsed         BLOB NOT NULL,
    parser_version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS symbols (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL,
    file_id        INTEGER NOT NULL,
    kind           TEXT NOT NULL,
    first_commit   INTEGER NOT NULL,
    last_commit    INTEGER
);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS symbols_file ON symbols(file_id);

CREATE TABLE IF NOT EXISTS edges (
    id             INTEGER PRIMARY KEY,
    src_file       INTEGER NOT NULL,
    dst_file       INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    resolution     INTEGER NOT NULL,
    UNIQUE (src_file, dst_file, kind)
);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src_file);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst_file);

CREATE TABLE IF NOT EXISTS edge_intervals (
    edge_id        INTEGER NOT NULL,
    born_commit    INTEGER NOT NULL,
    died_commit    INTEGER,
    PRIMARY KEY (edge_id, born_commit)
);
CREATE INDEX IF NOT EXISTS edge_intervals_died ON edge_intervals(died_commit);

CREATE TABLE IF NOT EXISTS cochange (
    a              INTEGER NOT NULL,
    b              INTEGER NOT NULL,
    n              INTEGER NOT NULL,
    w_support      REAL NOT NULL,
    w_decayed      REAL NOT NULL,
    conf_ab        REAL NOT NULL,
    conf_ba        REAL NOT NULL,
    lift           REAL NOT NULL,
    first_commit   INTEGER NOT NULL,
    last_commit    INTEGER NOT NULL,
    PRIMARY KEY (a, b)
);
CREATE INDEX IF NOT EXISTS cochange_a ON cochange(a, w_decayed DESC);
CREATE INDEX IF NOT EXISTS cochange_b ON cochange(b, w_decayed DESC);

CREATE TABLE IF NOT EXISTS ghosts (
    edge_id          INTEGER PRIMARY KEY,
    severed_commit   INTEGER NOT NULL,
    lifetime_days    INTEGER NOT NULL,
    cochanges_since  INTEGER NOT NULL,
    conf_since       REAL NOT NULL,
    score            REAL NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS commit_fts
    USING fts5(subject, body, content='commits', content_rowid='id');
"#;
