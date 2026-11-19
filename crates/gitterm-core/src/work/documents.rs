//! The documents of a workspace (TRU-191): what its agents wrote and the
//! plans and decisions kept in its repos, one entry per document across
//! worktrees. The overview's Documents section and the sidebar's Plans
//! mode show the same list.
//!
//! Sources, merged:
//! 1. every markdown file under the workspace's docs folder
//!    (`<home>/docs`, where a work's agents keep their working notes);
//! 2. the markdown files its chats (and their children) wrote or edited,
//!    from the chat facts (`ChatFacts::documents`);
//! 3. in each listed local repo, every checkout (main and linked
//!    worktrees): all markdown under `.plans/`, and under `docs/` only the
//!    well-known names ([`is_well_known_name`]).
//!
//! A document's identity is (git common dir, path within the checkout),
//! so the main checkout's `docs/DECISIONS.md` and a worktree's copy are
//! one entry whatever the repo list folds; outside git it is the absolute
//! path. The walk is bounded ([`MAX_DEPTH`], [`MAX_FILES`],
//! [`MAX_ENTRIES`]), skips hidden folders, and runs git only to resolve a
//! linked worktree's common dir and to list a repo's worktrees. Blocking:
//! never call [`scan`] on the UI thread. Missing files are skipped.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::summary::{folder_name as repo_name, WorktreeInfo};
use crate::chat_facts::is_markdown_path;
use crate::workers::checkout_root;

/// Host-owned Git discovery; calls remain lazy and in scan order.
pub trait GitDiscovery {
    fn list_worktrees(&mut self, repo: &Path) -> Result<Vec<WorktreeInfo>, String>;
    fn git_common_dir(&mut self, root: &Path) -> Result<PathBuf, String>;
}

/// Folder levels walked below `.plans/`, `docs/` or the docs folder.
pub const MAX_DEPTH: usize = 6;
/// Documents files gathered per scan, all sources together.
pub const MAX_FILES: usize = 2000;
/// Directory entries looked at per scan before the walk stops.
pub const MAX_ENTRIES: usize = 20_000;
/// Bytes read from a file to find its title.
const TITLE_BYTES: u64 = 8 * 1024;
/// File stems listed from a repo's `docs/` (any case); `ADR` is a prefix.
const WELL_KNOWN: &[&str] = &["DECISIONS", "PLAN", "PLANS", "DESIGN", "STATUS"];

/// Whether `file_name` is a well-known document name under `docs/`:
/// DECISIONS, PLAN, PLANS, DESIGN, STATUS or ADR* with a markdown
/// extension, any case.
pub fn is_well_known_name(file_name: &str) -> bool {
    let path = Path::new(file_name);
    if !is_markdown_path(path) {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let stem = stem.to_ascii_uppercase();
    WELL_KNOWN.contains(&stem.as_str()) || stem.starts_with("ADR")
}

/// What one scan reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanRequest {
    /// `<workspace home>/docs`: every markdown file in it.
    pub docs_dir: PathBuf,
    /// Markdown paths the workspace's chats wrote, from their facts.
    pub written: Vec<PathBuf>,
    /// The workspace's listed local repos (main checkouts).
    pub repos: Vec<PathBuf>,
    /// GitTerm task worktrees: never read, like agent scratch worktrees
    /// under `.claude/worktrees/` (TRU-180's exclusion).
    pub task_worktrees: Vec<PathBuf>,
}

/// Whether a checkout is scratch: a Claude Code agent worktree or a
/// GitTerm task worktree. Their files are copies of the repo's, not
/// documents of the work.
fn is_scratch_worktree(path: &Path, task_worktrees: &[PathBuf]) -> bool {
    let agent_scratch = path
        .components()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair[0].as_os_str() == ".claude" && pair[1].as_os_str() == "worktrees");
    agent_scratch || task_worktrees.iter().any(|tree| path.starts_with(tree))
}

/// Where one copy of a document lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopySource {
    /// In the workspace's docs folder, outside git.
    WorkDocs,
    /// In a git checkout: the repo's main checkout folder name, and for a
    /// linked worktree its branch (or folder name when detached).
    Repo {
        repo: String,
        worktree: Option<String>,
    },
    /// Outside git and the docs folder: the folder holding it.
    Elsewhere { folder: PathBuf },
}

impl CopySource {
    pub fn label(&self) -> String {
        match self {
            CopySource::WorkDocs => "work docs".to_string(),
            CopySource::Repo {
                repo,
                worktree: None,
            } => repo.clone(),
            CopySource::Repo {
                repo,
                worktree: Some(worktree),
            } => format!("{repo} \u{00B7} {worktree}"),
            CopySource::Elsewhere { folder } => folder.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentCopy {
    pub path: PathBuf,
    pub source: CopySource,
    pub modified: SystemTime,
}

/// One document and its copies across checkouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Identity: the git common dir and the path within the checkout, or
    /// the absolute path outside git.
    pub key: String,
    /// The newest copy's first heading, else its file name.
    pub title: String,
    /// The path within the checkout; in the docs folder, `docs/<path>`;
    /// elsewhere the file name.
    pub rel: String,
    /// Newest first (by modification time); never empty.
    pub copies: Vec<DocumentCopy>,
    /// More than one copy, and their contents are not all the same.
    pub differ: bool,
}

impl Document {
    pub fn newest(&self) -> &DocumentCopy {
        &self.copies[0]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    /// Newest first.
    pub documents: Vec<Document>,
    /// A bound stopped the walk; the list may be incomplete.
    pub truncated: bool,
}

/// One file found, before grouping.
#[derive(Debug, Clone)]
struct Found {
    path: PathBuf,
    modified: SystemTime,
    size: u64,
}

struct Budget {
    files: usize,
    entries: usize,
    truncated: bool,
}

impl Budget {
    fn new() -> Self {
        Self {
            files: MAX_FILES,
            entries: MAX_ENTRIES,
            truncated: false,
        }
    }
}

/// A checkout's identity: its root, its git common dir, and how a copy in
/// it is labelled.
#[derive(Debug, Clone)]
struct Checkout {
    root: PathBuf,
    common: PathBuf,
    repo: String,
    worktree: Option<String>,
}

struct Resolver<'a> {
    git: &'a mut dyn GitDiscovery,
    by_root: HashMap<PathBuf, Option<Checkout>>,
}

impl Resolver<'_> {
    /// Records a repo's checkouts from `git worktree list`; the first is
    /// the main checkout. Returns them, or the repo alone when git cannot
    /// list it (logged) or it is not in git.
    fn list_repo(&mut self, repo: &Path) -> Vec<PathBuf> {
        if checkout_root(repo).is_none() {
            return vec![repo.to_path_buf()];
        }
        let worktrees = match self.git.list_worktrees(repo) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => return vec![repo.to_path_buf()],
            Err(error) => {
                eprintln!("[work-documents] {error}; reading {} alone", repo.display());
                return vec![repo.to_path_buf()];
            }
        };
        let common = match self.git.git_common_dir(&worktrees[0].path) {
            Ok(common) => common,
            Err(error) => {
                eprintln!("[work-documents] {error}; reading {} alone", repo.display());
                return vec![repo.to_path_buf()];
            }
        };
        let name = repo_name(&worktrees[0].path);
        for (index, worktree) in worktrees.iter().enumerate() {
            let label = (index > 0).then(|| {
                worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| repo_name(&worktree.path))
            });
            self.by_root.insert(
                worktree.path.clone(),
                Some(Checkout {
                    root: worktree.path.clone(),
                    common: common.clone(),
                    repo: name.clone(),
                    worktree: label,
                }),
            );
        }
        worktrees
            .into_iter()
            .map(|worktree| worktree.path)
            .collect()
    }

    /// The checkout holding `path` (canonical), if it is in git.
    fn checkout_of(&mut self, path: &Path) -> Option<Checkout> {
        let root = checkout_root(path.parent()?)?;
        if let Some(known) = self.by_root.get(&root) {
            return known.clone();
        }
        let checkout = match self.git.git_common_dir(&root) {
            Ok(common) => {
                let main = (common.file_name().and_then(|n| n.to_str()) == Some(".git"))
                    .then(|| common.parent().map(Path::to_path_buf))
                    .flatten();
                let repo = repo_name(main.as_deref().unwrap_or(&root));
                let worktree = (main.as_deref() != Some(root.as_path())).then(|| repo_name(&root));
                Some(Checkout {
                    root: root.clone(),
                    common,
                    repo,
                    worktree,
                })
            }
            Err(error) => {
                eprintln!("[work-documents] {error}; listing files there by path");
                None
            }
        };
        self.by_root.insert(root, checkout.clone());
        checkout
    }
}

/// Gathers and groups the workspace's documents. Blocking.
pub fn scan(request: &ScanRequest, git: &mut dyn GitDiscovery) -> ScanResult {
    let mut budget = Budget::new();
    let mut resolver = Resolver {
        git,
        by_root: HashMap::new(),
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: Vec<Found> = Vec::new();

    let docs_dir = canonical(&request.docs_dir);
    walk(&docs_dir, 0, &|_| true, &mut budget, &mut seen, &mut found);
    for repo in &request.repos {
        for (index, checkout) in resolver.list_repo(repo).into_iter().enumerate() {
            // The first is the main checkout, always read.
            if index > 0 && is_scratch_worktree(&checkout, &request.task_worktrees) {
                continue;
            }
            walk(
                &checkout.join(".plans"),
                0,
                &|_| true,
                &mut budget,
                &mut seen,
                &mut found,
            );
            walk(
                &checkout.join("docs"),
                0,
                &is_well_known_name,
                &mut budget,
                &mut seen,
                &mut found,
            );
        }
    }
    for path in &request.written {
        if budget.files == 0 {
            budget.truncated = true;
            break;
        }
        if !is_markdown_path(path) || is_scratch_worktree(path, &request.task_worktrees) {
            continue;
        }
        // A file since deleted or moved is skipped.
        let Ok(path) = std::fs::canonicalize(path) else {
            continue;
        };
        if let Some(file) = found_file(&path) {
            if seen.insert(path) {
                budget.files -= 1;
                found.push(file);
            }
        }
    }

    let documents = group(found, &docs_dir, &mut resolver);
    ScanResult {
        documents,
        truncated: budget.truncated,
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn found_file(path: &Path) -> Option<Found> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(Found {
        path: path.to_path_buf(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: meta.len(),
    })
}

/// Markdown files below `dir` whose name passes `keep`, depth-first, at
/// most [`MAX_DEPTH`] levels down. Hidden entries and `_archive` folders
/// are skipped; a folder that cannot be read is skipped.
fn walk(
    dir: &Path,
    depth: usize,
    keep: &dyn Fn(&str) -> bool,
    budget: &mut Budget,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<Found>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<std::fs::DirEntry> = read.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if budget.entries == 0 || budget.files == 0 {
            budget.truncated = true;
            return;
        }
        budget.entries -= 1;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || name == "_archive" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if depth + 1 < MAX_DEPTH {
                walk(&path, depth + 1, keep, budget, seen, out);
            } else {
                budget.truncated = true;
            }
            continue;
        }
        if !kind.is_file() || !is_markdown_path(&path) || !keep(name) {
            continue;
        }
        let path = canonical(&path);
        if seen.contains(&path) {
            continue;
        }
        if let Some(file) = found_file(&path) {
            seen.insert(path);
            budget.files -= 1;
            out.push(file);
        }
    }
}

/// Groups files into documents by identity, newest first.
fn group(found: Vec<Found>, docs_dir: &Path, resolver: &mut Resolver<'_>) -> Vec<Document> {
    let mut by_key: HashMap<String, (String, Vec<(Found, CopySource)>)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for file in found {
        let checkout = resolver.checkout_of(&file.path);
        let (key, rel, source) = match checkout {
            Some(checkout) => {
                let rel = slash_path(file.path.strip_prefix(&checkout.root).unwrap_or(&file.path));
                (
                    format!("{}\u{0}{rel}", checkout.common.display()),
                    rel,
                    CopySource::Repo {
                        repo: checkout.repo,
                        worktree: checkout.worktree,
                    },
                )
            }
            None => {
                let key = file.path.display().to_string();
                match file.path.strip_prefix(docs_dir) {
                    Ok(below) => (
                        key,
                        format!("docs/{}", slash_path(below)),
                        CopySource::WorkDocs,
                    ),
                    Err(_) => {
                        let rel = file
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| key.clone());
                        let folder = file.path.parent().unwrap_or(Path::new("/")).to_path_buf();
                        (key, rel, CopySource::Elsewhere { folder })
                    }
                }
            }
        };
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (rel, Vec::new())
        });
        entry.1.push((file, source));
    }
    let mut documents: Vec<Document> = order
        .into_iter()
        .filter_map(|key| {
            let (rel, mut copies) = by_key.remove(&key)?;
            copies.sort_by(|a, b| {
                b.0.modified
                    .cmp(&a.0.modified)
                    .then_with(|| a.0.path.cmp(&b.0.path))
            });
            // Copies identical to the main checkout's are noise: keep the
            // main copy and the ones that differ from it.
            let main = copies
                .iter()
                .position(|(_, source)| matches!(source, CopySource::Repo { worktree: None, .. }));
            if let Some(main) = main {
                let main_file = copies[main].0.clone();
                let mut index = 0;
                copies.retain(|(file, _)| {
                    let keep = index == main || !same_content(&main_file, file);
                    index += 1;
                    keep
                });
            }
            let differ = copies_differ(&copies.iter().map(|(file, _)| file).collect::<Vec<_>>());
            let title = title_of(&copies[0].0.path);
            Some(Document {
                key,
                title,
                rel,
                differ,
                copies: copies
                    .into_iter()
                    .map(|(file, source)| DocumentCopy {
                        path: file.path,
                        source,
                        modified: file.modified,
                    })
                    .collect(),
            })
        })
        .collect();
    documents.sort_by(|a, b| {
        b.newest()
            .modified
            .cmp(&a.newest().modified)
            .then_with(|| a.rel.cmp(&b.rel))
    });
    documents
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether two copies hold the same bytes. A copy that cannot be read is
/// not the same.
fn same_content(a: &Found, b: &Found) -> bool {
    if a.size != b.size {
        return false;
    }
    match (std::fs::read(&a.path), std::fs::read(&b.path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether the copies' contents are not all the same: sizes first, then a
/// SHA-256 of each when the sizes match. A copy that cannot be read
/// counts as differing.
fn copies_differ(copies: &[&Found]) -> bool {
    let Some(first) = copies.first() else {
        return false;
    };
    if copies.iter().any(|copy| copy.size != first.size) {
        return true;
    }
    let digest = |path: &Path| {
        std::fs::read(path)
            .ok()
            .map(|bytes| ring::digest::digest(&ring::digest::SHA256, &bytes))
            .map(|digest| digest.as_ref().to_vec())
    };
    let Some(expected) = digest(&first.path) else {
        return copies.len() > 1;
    };
    copies[1..]
        .iter()
        .any(|copy| digest(&copy.path).as_ref() != Some(&expected))
}

/// The first markdown heading in the file's first bytes, else its name.
fn title_of(path: &Path) -> String {
    let mut head = String::new();
    if let Ok(file) = std::fs::File::open(path) {
        let mut bytes = Vec::new();
        if file.take(TITLE_BYTES).read_to_end(&mut bytes).is_ok() {
            head = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    heading_in(&head).unwrap_or_else(|| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    })
}

/// The text of the first ATX heading (`# Title`, `## Title`, ...).
pub fn heading_in(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line.trim_start_matches('#');
        let level = line.len() - rest.len();
        let heading = (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' '));
        let title = rest.trim().trim_end_matches('#').trim();
        (heading && !title.is_empty()).then(|| title.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct NoGit;
    impl GitDiscovery for NoGit {
        fn list_worktrees(&mut self, _: &Path) -> Result<Vec<WorktreeInfo>, String> {
            panic!("non-Git fixture must not discover worktrees")
        }
        fn git_common_dir(&mut self, _: &Path) -> Result<PathBuf, String> {
            panic!("non-Git fixture must not resolve a common directory")
        }
    }

    fn write(path: &Path, text: &str, age_secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        let at = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn scratch_worktrees_are_recognised() {
        let tasks = vec![PathBuf::from("/tasks/api-1")];
        assert!(is_scratch_worktree(
            Path::new("/r/api/.claude/worktrees/agent-1"),
            &tasks
        ));
        assert!(is_scratch_worktree(Path::new("/tasks/api-1"), &tasks));
        assert!(!is_scratch_worktree(Path::new("/r/api"), &tasks));
        assert!(!is_scratch_worktree(Path::new("/r/api-wt"), &tasks));
    }

    #[test]
    fn bounded_walk_preserves_order_filters_and_entry_accounting() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for name in [
            ".hidden.md",
            ".hidden/secret.md",
            "_archive/old.md",
            "b.md",
            "a/first.markdown",
            "a/second.MD",
            "ignored.md",
            "plain.txt",
        ] {
            write(&root.join(name), "# Synthetic\n", 10);
        }
        write(&root.join(".gitignore"), "ignored.md\n", 10);
        let mut budget = Budget::new();
        let mut found = Vec::new();
        walk(
            &root,
            0,
            &|_| true,
            &mut budget,
            &mut HashSet::new(),
            &mut found,
        );
        let paths: Vec<_> = found
            .iter()
            .map(|f| slash_path(f.path.strip_prefix(&root).unwrap()))
            .collect();
        assert_eq!(
            paths,
            ["a/first.markdown", "a/second.MD", "b.md", "ignored.md"]
        );
        assert!(!budget.truncated);
        assert_eq!(budget.entries, MAX_ENTRIES - 10);

        let mut budget = Budget {
            entries: 2,
            ..Budget::new()
        };
        let mut found = Vec::new();
        walk(
            &root,
            0,
            &|_| true,
            &mut budget,
            &mut HashSet::new(),
            &mut found,
        );
        assert!(found.is_empty());
        assert_eq!(budget.entries, 0);
        assert_eq!(budget.files, MAX_FILES);
        assert!(budget.truncated);
    }

    #[test]
    fn depth_limit_and_missing_directories_keep_existing_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let mut dir = root.clone();
        for _ in 0..=MAX_DEPTH {
            write(&dir.join("level.md"), "# Synthetic\n", 10);
            dir = dir.join("child");
        }
        let mut budget = Budget::new();
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        walk(&root, 0, &|_| true, &mut budget, &mut seen, &mut found);
        assert_eq!(found.len(), MAX_DEPTH);
        assert!(budget.truncated);
        assert_eq!(
            found.first().unwrap().path.components().count(),
            root.components().count() + MAX_DEPTH
        );
        let before = (budget.entries, budget.files, found.len());
        walk(
            &temp.path().join("missing"),
            0,
            &|_| true,
            &mut budget,
            &mut seen,
            &mut found,
        );
        assert_eq!((budget.entries, budget.files, found.len()), before);
    }

    #[test]
    fn file_budget_is_shared_in_docs_repo_then_written_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for i in 0..MAX_FILES - 1 {
            write(
                &root.join(format!("work/docs/{i:04}.md")),
                "# Synthetic\n",
                10,
            );
        }
        write(&root.join("repo/.plans/a.md"), "# First plan\n", 10);
        write(&root.join("repo/.plans/b.md"), "# Second plan\n", 10);
        write(&root.join("written.md"), "# Written\n", 10);
        let result = scan(
            &ScanRequest {
                docs_dir: root.join("work/docs"),
                repos: vec![root.join("repo")],
                written: vec![root.join("written.md")],
                task_worktrees: Vec::new(),
            },
            &mut NoGit,
        );
        assert_eq!(result.documents.len(), MAX_FILES);
        assert!(result.truncated);
        assert!(result.documents.iter().any(|d| d.title == "First plan"));
        assert!(!result
            .documents
            .iter()
            .any(|d| d.title == "Second plan" || d.title == "Written"));
    }

    #[cfg(unix)]
    #[test]
    fn written_paths_bypass_hidden_archive_and_walk_symlink_filters() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for name in [
            "docs/visible.md",
            "docs/.hidden/secret.md",
            "docs/_archive/old.md",
            "elsewhere/target.md",
            "task/excluded.md",
        ] {
            write(&root.join(name), "# Synthetic\n", 10);
        }
        symlink(root.join("elsewhere/target.md"), root.join("docs/link.md")).unwrap();
        symlink(root.join("elsewhere"), root.join("docs/linked-dir")).unwrap();
        let result = scan(
            &ScanRequest {
                docs_dir: root.join("docs"),
                repos: Vec::new(),
                written: vec![
                    root.join("docs/.hidden/secret.md"),
                    root.join("docs/_archive/old.md"),
                    root.join("docs/link.md"),
                    root.join("task/excluded.md"),
                    root.join("missing.md"),
                ],
                task_worktrees: vec![root.join("task")],
            },
            &mut NoGit,
        );
        let mut paths: Vec<_> = result
            .documents
            .iter()
            .map(|d| slash_path(d.newest().path.strip_prefix(&root).unwrap()))
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "docs/.hidden/secret.md",
                "docs/_archive/old.md",
                "docs/visible.md",
                "elsewhere/target.md"
            ]
        );
        assert!(!result.truncated);
    }
    struct Discovery {
        list: Result<Vec<WorktreeInfo>, String>,
        common: std::collections::VecDeque<Result<PathBuf, String>>,
        calls: Vec<(&'static str, PathBuf)>,
    }

    impl GitDiscovery for Discovery {
        fn list_worktrees(&mut self, repo: &Path) -> Result<Vec<WorktreeInfo>, String> {
            self.calls.push(("list", repo.to_path_buf()));
            self.list.clone()
        }
        fn git_common_dir(&mut self, root: &Path) -> Result<PathBuf, String> {
            self.calls.push(("common", root.to_path_buf()));
            self.common
                .pop_front()
                .expect("unexpected common-dir lookup")
        }
    }

    #[test]
    fn discovery_fallbacks_preserve_lazy_calls_and_negative_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join(".plans/a.md"), "# A\n", 10);
        write(&repo.join(".plans/b.md"), "# B\n", 10);
        let listed = vec![WorktreeInfo {
            path: repo.clone(),
            branch: Some("main".into()),
        }];
        for (list, common, expected, repo_source) in [
            (
                Ok(Vec::new()),
                vec![Ok(repo.join(".git"))],
                vec!["list", "common"],
                true,
            ),
            (
                Err("synthetic list failure".into()),
                vec![Ok(repo.join(".git"))],
                vec!["list", "common"],
                true,
            ),
            (
                Ok(listed.clone()),
                vec![
                    Err("synthetic first common failure".into()),
                    Ok(repo.join(".git")),
                ],
                vec!["list", "common", "common"],
                true,
            ),
            (
                Err("synthetic list failure".into()),
                vec![Err("synthetic common failure".into())],
                vec!["list", "common"],
                false,
            ),
        ] {
            let mut git = Discovery {
                list,
                common: common.into(),
                calls: Vec::new(),
            };
            let result = scan(
                &ScanRequest {
                    docs_dir: root.join("missing/docs"),
                    repos: vec![repo.clone()],
                    ..ScanRequest::default()
                },
                &mut git,
            );
            assert_eq!(result.documents.len(), 2);
            assert!(!result.truncated);
            assert_eq!(
                git.calls,
                expected
                    .into_iter()
                    .map(|kind| (kind, repo.clone()))
                    .collect::<Vec<_>>()
            );
            assert!(git.common.is_empty());
            for doc in result.documents {
                assert_eq!(
                    matches!(doc.newest().source, CopySource::Repo { .. }),
                    repo_source
                );
            }
        }
    }

    #[test]
    fn first_checkout_is_read_even_when_scratch_and_later_scratch_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let main = root.join(".claude/worktrees/first");
        let later = root.join("later-task");
        let other = root.join("other");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        for (path, name) in [(&main, "first"), (&later, "excluded"), (&other, "other")] {
            write(&path.join(format!(".plans/{name}.md")), "# Synthetic\n", 10);
        }
        for path in [&later, &other] {
            std::fs::write(path.join(".git"), "gitdir: synthetic\n").unwrap();
        }
        let mut git = Discovery {
            list: Ok([&main, &later, &other]
                .into_iter()
                .map(|path| WorktreeInfo {
                    path: path.clone(),
                    branch: None,
                })
                .collect()),
            common: [Ok(main.join(".git"))].into(),
            calls: Vec::new(),
        };
        let result = scan(
            &ScanRequest {
                docs_dir: root.join("missing"),
                repos: vec![main.clone()],
                task_worktrees: vec![main.clone(), later.clone()],
                written: vec![later.join(".plans/excluded.md")],
            },
            &mut git,
        );
        let mut rels: Vec<_> = result.documents.iter().map(|d| d.rel.as_str()).collect();
        rels.sort();
        assert_eq!(rels, [".plans/first.md", ".plans/other.md"]);
        assert_eq!(git.calls, [("list", main.clone()), ("common", main)]);
    }

    #[test]
    fn exhausted_walk_budget_does_not_skip_or_deduplicate_repo_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for i in 0..MAX_FILES {
            write(&root.join(format!("docs/{i:04}.md")), "# Synthetic\n", 10);
        }
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join(".plans/unread.md"), "# Unread\n", 10);
        let mut git = Discovery {
            list: Ok(vec![WorktreeInfo {
                path: repo.clone(),
                branch: None,
            }]),
            common: [Ok(repo.join(".git")), Ok(repo.join(".git"))].into(),
            calls: Vec::new(),
        };
        let result = scan(
            &ScanRequest {
                docs_dir: root.join("docs"),
                repos: vec![repo.clone(), repo.clone()],
                ..ScanRequest::default()
            },
            &mut git,
        );
        assert_eq!(result.documents.len(), MAX_FILES);
        assert!(result.truncated);
        assert_eq!(
            git.calls,
            ["list", "common", "list", "common"].map(|kind| (kind, repo.clone()))
        );
    }
    #[test]
    fn titles_are_bounded_and_same_size_copies_compare_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.md");
        let b = temp.path().join("b.md");
        write(&a, "# AAAA\n", 10);
        write(&b, "# BBBB\n", 10);
        let first = found_file(&a).unwrap();
        let second = found_file(&b).unwrap();
        assert_eq!(first.size, second.size);
        assert!(copies_differ(&[&first, &second]));
        assert!(!same_content(&first, &second));
        std::fs::write(&b, "# AAAA\n").unwrap();
        assert!(!copies_differ(&[&first, &second]));
        assert!(same_content(&first, &second));
        std::fs::remove_file(&b).unwrap();
        assert!(copies_differ(&[&first, &second]));
        assert!(!same_content(&first, &second));
        assert_eq!(title_of(&b), "b.md");
        std::fs::write(
            &a,
            format!(
                "{}\n# Beyond title limit\n",
                "x".repeat(TITLE_BYTES as usize)
            ),
        )
        .unwrap();
        assert_eq!(title_of(&a), "a.md");
    }
}
