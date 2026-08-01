//! Version-control access, isolated behind a small trait so the backend can
//! be swapped without touching the walker. The current implementation uses
//! git2; the plan allows a gix backend behind the same interface.

use anyhow::{Context, Result};
use git2::{Delta, DiffFindOptions, DiffOptions, Oid, Patch, Repository, Sort};

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub oid: Vec<u8>,
    pub parent_oid: Option<Vec<u8>>,
    pub author_time: i64,
    pub author: String,
    pub subject: String,
    pub body: String,
    pub is_merge: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Added { path: String },
    Modified { path: String },
    Deleted { path: String },
    Renamed { from: String, to: String },
}

impl Change {
    /// The path the file has after this change (the deleted path for deletes).
    pub fn path(&self) -> &str {
        match self {
            Change::Added { path } | Change::Modified { path } | Change::Deleted { path } => path,
            Change::Renamed { to, .. } => to,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub change: Change,
    /// Blob oid of the new content; None for deletions.
    pub new_blob: Option<Vec<u8>>,
    pub old_blob: Option<Vec<u8>>,
    pub lines_added: i64,
    pub lines_removed: i64,
    /// Contents of added and removed lines, for import-only detection.
    /// Empty when the file is binary or the change is enormous.
    pub changed_lines: Vec<String>,
    /// Rename identity confidence in [0, 1]; 1.0 for non-renames.
    pub confidence: f64,
}

pub trait Vcs {
    fn head_oid(&self) -> Result<Vec<u8>>;
    /// Commits oldest to newest. `first_parent` simplifies history to the
    /// first-parent chain. `hide` excludes a rev and its ancestors (used by
    /// --since and incremental indexing). `since_time` drops commits older
    /// than a unix timestamp.
    fn commits(
        &self,
        first_parent: bool,
        hide: Option<&str>,
        since_time: Option<i64>,
    ) -> Result<Vec<CommitInfo>>;
    fn diff_commit(
        &self,
        oid: &[u8],
        parent: Option<&[u8]>,
        rename_threshold: f64,
    ) -> Result<Vec<FileChange>>;
    fn blob(&self, oid: &[u8]) -> Result<Vec<u8>>;
    /// All (path, blob_oid) pairs in a commit's tree. Used to seed the live
    /// state when indexing starts mid-history.
    fn tree_files(&self, oid: &[u8]) -> Result<Vec<(String, Vec<u8>)>>;
}

pub struct GitVcs {
    repo: Repository,
}

impl GitVcs {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let repo = Repository::discover(path)
            .with_context(|| format!("not a git repository: {}", path.display()))?;
        Ok(GitVcs { repo })
    }

    pub fn resolve_rev(&self, rev: &str) -> Result<Vec<u8>> {
        let obj = self.repo.revparse_single(rev)?;
        let commit = obj.peel_to_commit()?;
        Ok(commit.id().as_bytes().to_vec())
    }
}

const MAX_CHANGED_LINES_KEPT: usize = 4_000;

impl Vcs for GitVcs {
    fn head_oid(&self) -> Result<Vec<u8>> {
        let head = self.repo.head()?.peel_to_commit()?;
        Ok(head.id().as_bytes().to_vec())
    }

    fn commits(
        &self,
        first_parent: bool,
        hide: Option<&str>,
        since_time: Option<i64>,
    ) -> Result<Vec<CommitInfo>> {
        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        if let Some(rev) = hide {
            let obj = self.repo.revparse_single(rev)?;
            walk.hide(obj.peel_to_commit()?.id())?;
        }
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
        if first_parent {
            walk.simplify_first_parent()?;
        }
        let mut out = Vec::new();
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            let time = commit.author().when().seconds();
            if let Some(cutoff) = since_time {
                if time < cutoff {
                    continue;
                }
            }
            let message = commit.message().unwrap_or("");
            let subject = commit.summary().unwrap_or("").to_string();
            let body = message
                .strip_prefix(subject.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(CommitInfo {
                oid: oid.as_bytes().to_vec(),
                parent_oid: commit.parent_id(0).ok().map(|p| p.as_bytes().to_vec()),
                author_time: time,
                author: commit.author().name().unwrap_or("").to_string(),
                subject,
                body,
                is_merge: commit.parent_count() > 1,
            });
        }
        Ok(out)
    }

    fn diff_commit(
        &self,
        oid: &[u8],
        parent: Option<&[u8]>,
        rename_threshold: f64,
    ) -> Result<Vec<FileChange>> {
        let commit = self.repo.find_commit(Oid::from_bytes(oid)?)?;
        let tree = commit.tree()?;
        let parent_tree = match parent {
            Some(p) => Some(self.repo.find_commit(Oid::from_bytes(p)?)?.tree()?),
            None => None,
        };
        let mut opts = DiffOptions::new();
        opts.include_typechange(true);
        let mut diff =
            self.repo
                .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
        let mut find = DiffFindOptions::new();
        find.renames(true);
        find.rename_threshold((rename_threshold * 100.0) as u16);
        diff.find_similar(Some(&mut find))?;

        let mut out = Vec::new();
        let n = diff.deltas().len();
        for i in 0..n {
            let delta = diff.get_delta(i).unwrap();
            // Submodules are commits-in-trees, not files.
            if delta.new_file().mode() == git2::FileMode::Commit
                || delta.old_file().mode() == git2::FileMode::Commit
            {
                continue;
            }
            let old_path = delta
                .old_file()
                .path()
                .map(|p| p.to_string_lossy().replace('\\', "/"));
            let new_path = delta
                .new_file()
                .path()
                .map(|p| p.to_string_lossy().replace('\\', "/"));
            let (change, confidence) = match delta.status() {
                Delta::Added | Delta::Copied => (
                    Change::Added {
                        path: match new_path {
                            Some(p) => p,
                            None => continue,
                        },
                    },
                    1.0,
                ),
                Delta::Deleted => (
                    Change::Deleted {
                        path: match old_path {
                            Some(p) => p,
                            None => continue,
                        },
                    },
                    1.0,
                ),
                Delta::Modified | Delta::Typechange => (
                    Change::Modified {
                        path: match new_path {
                            Some(p) => p,
                            None => continue,
                        },
                    },
                    1.0,
                ),
                Delta::Renamed => {
                    let from = match old_path {
                        Some(p) => p,
                        None => continue,
                    };
                    let to = match new_path {
                        Some(p) => p,
                        None => continue,
                    };
                    // git2 does not expose the similarity score, so derive a
                    // coarse identity confidence: content-identical renames
                    // are certain, same-directory or same-stem renames are
                    // strong, cross-directory renames with a new name are
                    // the coin flips downstream metrics may discount.
                    let identical = delta.old_file().id() == delta.new_file().id();
                    let same_dir = parent_dir(&from) == parent_dir(&to);
                    let same_stem = file_stem(&from) == file_stem(&to);
                    let conf = if identical {
                        1.0
                    } else if same_dir || same_stem {
                        0.85
                    } else {
                        0.6
                    };
                    (Change::Renamed { from, to }, conf)
                }
                _ => continue,
            };

            let is_binary = delta.new_file().is_binary() || delta.old_file().is_binary();
            let new_blob = match &change {
                Change::Deleted { .. } => None,
                _ => Some(delta.new_file().id().as_bytes().to_vec()),
            };
            let old_blob = match delta.status() {
                Delta::Added => None,
                _ => Some(delta.old_file().id().as_bytes().to_vec()),
            };
            let (la, lr, changed_lines) = if is_binary {
                (0, 0, Vec::new())
            } else {
                match Patch::from_diff(&diff, i)? {
                    Some(patch) => {
                        let (_, add, del) = patch.line_stats()?;
                        let mut lines = Vec::new();
                        if add + del <= MAX_CHANGED_LINES_KEPT {
                            for h in 0..patch.num_hunks() {
                                let count = patch.num_lines_in_hunk(h)?;
                                for l in 0..count {
                                    let line = patch.line_in_hunk(h, l)?;
                                    let origin = line.origin();
                                    if origin == '+' || origin == '-' {
                                        lines.push(
                                            String::from_utf8_lossy(line.content())
                                                .trim_end()
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                        }
                        (add as i64, del as i64, lines)
                    }
                    None => (0, 0, Vec::new()),
                }
            };

            out.push(FileChange {
                change,
                new_blob,
                old_blob,
                lines_added: la,
                lines_removed: lr,
                changed_lines,
                confidence,
            });
        }
        Ok(out)
    }

    fn blob(&self, oid: &[u8]) -> Result<Vec<u8>> {
        let blob = self.repo.find_blob(Oid::from_bytes(oid)?)?;
        Ok(blob.content().to_vec())
    }

    fn tree_files(&self, oid: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
        let commit = self.repo.find_commit(Oid::from_bytes(oid)?)?;
        let tree = commit.tree()?;
        let mut out = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                if let Some(name) = entry.name() {
                    out.push((format!("{root}{name}"), entry.id().as_bytes().to_vec()));
                }
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(out)
    }
}

pub fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

pub fn file_stem(path: &str) -> &str {
    let base = match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    match base.find('.') {
        Some(i) => &base[..i],
        None => base,
    }
}
