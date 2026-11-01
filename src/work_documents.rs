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

use crate::chat_facts::is_markdown_path;
use crate::workspace_repos::{checkout_root, git_common_dir, repo_name};
use crate::worktree_follow;

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

#[derive(Default)]
struct Resolver {
    by_root: HashMap<PathBuf, Option<Checkout>>,
}

impl Resolver {
    /// Records a repo's checkouts from `git worktree list`; the first is
    /// the main checkout. Returns them, or the repo alone when git cannot
    /// list it (logged) or it is not in git.
    fn list_repo(&mut self, repo: &Path) -> Vec<PathBuf> {
        if checkout_root(repo).is_none() {
            return vec![repo.to_path_buf()];
        }
        let worktrees = match worktree_follow::list_worktrees(repo) {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => return vec![repo.to_path_buf()],
            Err(error) => {
                eprintln!("[work-documents] {error}; reading {} alone", repo.display());
                return vec![repo.to_path_buf()];
            }
        };
        let common = match git_common_dir(&worktrees[0].path) {
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
        let checkout = match git_common_dir(&root) {
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
pub fn scan(request: &ScanRequest) -> ScanResult {
    let mut budget = Budget::new();
    let mut resolver = Resolver::default();
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
fn group(found: Vec<Found>, docs_dir: &Path, resolver: &mut Resolver) -> Vec<Document> {
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

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
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
    fn well_known_names_under_docs() {
        for name in [
            "DECISIONS.md",
            "decisions.md",
            "Plan.markdown",
            "PLANS.md",
            "design.MD",
            "STATUS.md",
            "ADR-0001-storage.md",
            "adr.md",
        ] {
            assert!(is_well_known_name(name), "{name}");
        }
        for name in [
            "README.md",
            "notes.md",
            "DECISIONS.txt",
            "DECISIONS",
            "my-plan.md",
            "HANDOVER.md",
        ] {
            assert!(!is_well_known_name(name), "{name}");
        }
    }

    #[test]
    fn headings_give_titles() {
        assert_eq!(
            heading_in("intro\n## Storage plan ##\n# Later"),
            Some("Storage plan".into())
        );
        assert_eq!(heading_in("#hashtag\n#\n# Real\n"), Some("Real".into()));
        assert_eq!(heading_in("no heading"), None);
    }

    #[test]
    fn copies_across_worktrees_are_one_document_with_the_newest_and_the_differ_flag() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let api = root.join("org").join("api");
        init_repo(&api);
        // An agent-made worktree beside the repo, and a second one.
        let agent_made = root.join("org").join(".worktrees").join("api-feature");
        git(
            &api,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                agent_made.to_str().unwrap(),
            ],
        );
        let other = root.join("org").join("api-other");
        git(
            &api,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                other.to_str().unwrap(),
            ],
        );

        // DECISIONS.md: three copies, the worktree's newest, contents differ.
        write(
            &api.join("docs/topic/DECISIONS.md"),
            "# Decisions\nold\n",
            300,
        );
        write(
            &agent_made.join("docs/topic/DECISIONS.md"),
            "# Decisions\nnew one\n",
            10,
        );
        write(
            &other.join("docs/topic/DECISIONS.md"),
            "# Decisions\nold\n",
            200,
        );
        // PLAN.md: two identical copies.
        write(&api.join(".plans/rollout.md"), "# Rollout\n", 100);
        write(&other.join(".plans/rollout.md"), "# Rollout\n", 50);
        // Not listed: a docs file without a well-known name, unless a chat
        // wrote it.
        write(&api.join("docs/notes.md"), "# Notes\n", 20);
        write(&api.join("docs/guide.md"), "# Guide\n", 30);
        // The work's own docs folder, outside git.
        let home = root.join("work").join("example");
        write(&home.join("docs/status.md"), "Status, no heading\n", 5);
        // A chat wrote a file outside every repo, and one since deleted.
        write(&root.join("elsewhere/scratch.md"), "# Scratch\n", 400);

        let result = scan(&ScanRequest {
            docs_dir: home.join("docs"),
            written: vec![
                api.join("docs/guide.md"),
                root.join("elsewhere/scratch.md"),
                root.join("gone/missing.md"),
                api.join("src/main.rs"),
            ],
            repos: vec![home.clone(), api.clone()],
            task_worktrees: Vec::new(),
        });
        assert!(!result.truncated);
        let rels: Vec<&str> = result.documents.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "docs/status.md",
                "docs/topic/DECISIONS.md",
                "docs/guide.md",
                ".plans/rollout.md",
                "scratch.md",
            ]
        );

        let status = &result.documents[0];
        assert_eq!(status.title, "status.md");
        assert_eq!(status.newest().source, CopySource::WorkDocs);
        assert_eq!(status.copies.len(), 1);
        assert!(!status.differ);

        let decisions = &result.documents[1];
        assert_eq!(decisions.title, "Decisions");
        // The "other" worktree's copy equals the main checkout's: hidden.
        assert_eq!(decisions.copies.len(), 2);
        assert!(decisions.differ);
        assert_eq!(
            decisions.newest().path,
            agent_made.join("docs/topic/DECISIONS.md")
        );
        assert_eq!(
            decisions.newest().source,
            CopySource::Repo {
                repo: "api".into(),
                worktree: Some("feature".into())
            }
        );
        assert_eq!(decisions.newest().source.label(), "api \u{00B7} feature");
        assert_eq!(
            decisions.copies[1].path,
            api.join("docs/topic/DECISIONS.md")
        );
        assert_eq!(decisions.copies[1].source.label(), "api");

        // The identical copy in another worktree is hidden: no copies line.
        let rollout = &result.documents[3];
        assert_eq!(rollout.copies.len(), 1);
        assert!(!rollout.differ);
        assert_eq!(rollout.newest().path, api.join(".plans/rollout.md"));

        let scratch = &result.documents[4];
        assert_eq!(
            scratch.newest().source,
            CopySource::Elsewhere {
                folder: root.join("elsewhere")
            }
        );

        // Found from a worktree alone (a chat wrote there; the repo is not
        // listed), the copy still joins its document's identity.
        let only_written = scan(&ScanRequest {
            docs_dir: home.join("docs"),
            written: vec![
                agent_made.join("docs/topic/DECISIONS.md"),
                api.join("docs/topic/DECISIONS.md"),
            ],
            repos: Vec::new(),
            task_worktrees: Vec::new(),
        });
        let decisions: Vec<&Document> = only_written
            .documents
            .iter()
            .filter(|d| d.rel == "docs/topic/DECISIONS.md")
            .collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].copies.len(), 2);
        assert_eq!(
            decisions[0].newest().source,
            CopySource::Repo {
                repo: "api".into(),
                worktree: Some("api-feature".into())
            }
        );
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
    fn agent_scratch_and_task_worktrees_and_identical_copies_do_not_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let api = root.join("api");
        init_repo(&api);
        let add = |path: &Path, branch: &str| {
            git(
                &api,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        };
        let scratch = api.join(".claude/worktrees/agent-1");
        let task = root.join("tasks/api-ops-1");
        let same = root.join("api-same");
        let changed = root.join("api-changed");
        add(&scratch, "scratch");
        add(&task, "task");
        add(&same, "same");
        add(&changed, "changed");
        for (dir, text, age) in [
            (&api, "# Plan\n", 100),
            (&scratch, "# Plan\nscratch edit\n", 1),
            (&task, "# Plan\ntask edit\n", 2),
            (&same, "# Plan\n", 50),
            (&changed, "# Plan\nreal edit\n", 20),
        ] {
            write(&dir.join(".plans/plan.md"), text, age);
        }
        write(&api.join(".plans/solo.md"), "# Solo\n", 90);
        write(&same.join(".plans/solo.md"), "# Solo\n", 40);

        let result = scan(&ScanRequest {
            docs_dir: root.join("none/docs"),
            written: vec![scratch.join(".plans/plan.md")],
            repos: vec![api.clone()],
            task_worktrees: vec![task.clone()],
        });
        let plan = result
            .documents
            .iter()
            .find(|d| d.rel == ".plans/plan.md")
            .unwrap();
        // Main plus the one real difference; scratch, task and the
        // identical copy are gone.
        let paths: Vec<&Path> = plan.copies.iter().map(|c| c.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![
                changed.join(".plans/plan.md").as_path(),
                api.join(".plans/plan.md").as_path()
            ]
        );
        assert!(plan.differ);

        // Every copy identical: one copy, no differ flag.
        let solo = result
            .documents
            .iter()
            .find(|d| d.rel == ".plans/solo.md")
            .unwrap();
        assert_eq!(solo.copies.len(), 1);
        assert!(!solo.differ);
    }
}
