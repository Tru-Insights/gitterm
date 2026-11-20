use std::cmp::Ordering;
use std::path::PathBuf;

use crate::git_facts::{BaseChoice, DiffSource};

/// What a changed file is, from its path alone ([`classify_path`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    Code,
    Config,
    Docs,
    Test,
    Generated,
}

impl FileKind {
    /// Display and admission order.
    pub const ALL: [FileKind; 5] = [
        FileKind::Code,
        FileKind::Config,
        FileKind::Docs,
        FileKind::Test,
        FileKind::Generated,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileKind::Code => "code",
            FileKind::Config => "config",
            FileKind::Docs => "docs",
            FileKind::Test => "test",
            FileKind::Generated => "generated",
        }
    }

    /// Hidden by the pane's "Hide tests & generated" toggle.
    pub fn is_noise(self) -> bool {
        matches!(self, FileKind::Test | FileKind::Generated)
    }

    pub(super) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .expect("every kind is in ALL")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    /// Binary content (added, modified, renamed or untracked): no lines.
    Binary,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Untracked => 'U',
            FileStatus::Binary => 'B',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Untracked => "untracked",
            FileStatus::Binary => "binary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOp {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLine {
    pub op: LineOp,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewHunk {
    /// The `@@ -a,b +c,d @@ context` line.
    pub header: String,
    pub lines: Vec<ReviewLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub path: String,
    /// The path before a rename.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub kind: FileKind,
    pub additions: usize,
    pub deletions: usize,
    /// Cut at [`FILE_LINE_CAP`](super::budget::FILE_LINE_CAP) / [`FILE_BYTE_CAP`](super::budget::FILE_BYTE_CAP) or the total caps.
    pub truncated: bool,
    /// Listed but not shown: past [`ADMIT_LINE_BUDGET`](super::budget::ADMIT_LINE_BUDGET) or the total caps.
    pub omitted: bool,
    /// Why a file has no lines when that is not binary or omitted (a nested
    /// repository, a file that could not be read).
    pub note: Option<String>,
    pub hunks: Vec<ReviewHunk>,
    /// The version shown, for viewed marks: `git hash-object` of the
    /// working-tree file (Uncommitted) or its blob id (committed sources);
    /// `deleted:<old blob>` for a deletion, `commit:<id>` for a submodule.
    /// `None` when it could not be hashed: the file cannot be marked viewed.
    pub content_id: Option<String>,
}

impl ReviewFile {
    pub fn shown_lines(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindTotals {
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
}

/// Listed files, additions and deletions per [`FileKind`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewTotals {
    by_kind: [KindTotals; 5],
}

impl ReviewTotals {
    pub fn of(files: &[ReviewFile]) -> Self {
        let mut totals = ReviewTotals::default();
        for file in files {
            let slot = &mut totals.by_kind[file.kind.index()];
            slot.files += 1;
            slot.additions += file.additions;
            slot.deletions += file.deletions;
        }
        totals
    }

    pub fn get(&self, kind: FileKind) -> KindTotals {
        self.by_kind[kind.index()]
    }

    pub fn all(&self) -> KindTotals {
        self.by_kind
            .iter()
            .fold(KindTotals::default(), |sum, kind| KindTotals {
                files: sum.files + kind.files,
                additions: sum.additions + kind.additions,
                deletions: sum.deletions + kind.deletions,
            })
    }
}

/// The checkout a review reads, as the overview row names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub checkout: PathBuf,
    pub repo: String,
    pub branch: String,
    /// Whose branch it is ("coordinator", "matches PRJ-101").
    pub owner: String,
    /// The row's base and where it came from.
    pub base: Option<BaseChoice>,
}

/// One commit on the reviewed branch, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCommit {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    pub author: String,
    pub authored_at: i64,
}

/// A selected commit range. `first` and `last` are normalized oldest to newest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRange {
    pub first: String,
    pub last: String,
    /// A first click is waiting for a second endpoint; a completed range is not.
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSource {
    Uncommitted,
    VsBase,
    Commits(Option<CommitRange>),
    Turns { chat: String, selected: Option<u64> },
}

impl From<DiffSource> for ReviewSource {
    fn from(source: DiffSource) -> Self {
        match source {
            DiffSource::Uncommitted => Self::Uncommitted,
            DiffSource::VsBase => Self::VsBase,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDiff {
    pub checkout: PathBuf,
    pub branch: String,
    pub base: Option<BaseChoice>,
    pub source: DiffSource,
    /// Short merge-base commit for a vs-base diff.
    pub merge_base: Option<String>,
    /// In tree order (folders before files at each level).
    pub files: Vec<ReviewFile>,
    pub totals: ReviewTotals,
    /// Files in the diff past [`FILE_LIST_CAP`](super::budget::FILE_LIST_CAP), not listed.
    pub unlisted: usize,
}

impl ReviewDiff {
    pub fn file(&self, path: &str) -> Option<&ReviewFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

// ---------------------------------------------------------------------------
// Path classifier.
//
// Ported from T3 Code's `packages/shared/src/changedFileKind.ts`
// (MIT License, Copyright (c) 2026 T3 Tools Inc.), without regexes. T3's
// "mocks" (fixtures, snapshots, stubs) fold into `Test`, and `Config` is
// GitTerm's addition: data and tool configuration that is not prose.

const GENERATED_FILES: [&str; 20] = [
    "bun.lock",
    "bun.lockb",
    "cargo.lock",
    "composer.lock",
    "deno.lock",
    "flake.lock",
    "gemfile.lock",
    "go.sum",
    "gradle.lockfile",
    "mix.lock",
    "npm-shrinkwrap.json",
    "package-lock.json",
    "packages.lock.json",
    "pipfile.lock",
    "pnpm-lock.yaml",
    "podfile.lock",
    "poetry.lock",
    "pubspec.lock",
    "uv.lock",
    "yarn.lock",
];
const GENERATED_SUFFIXES: [&str; 10] = [
    ".g.dart",
    ".pb.go",
    "_pb2.py",
    "_pb2_grpc.py",
    ".pb.cc",
    ".pb.h",
    ".min.js",
    ".min.css",
    ".js.map",
    ".css.map",
];
const GENERATED_DIRS: [&str; 2] = ["__generated__", "generated"];
const MOCK_WORDS: [&str; 8] = [
    "mock", "mocks", "fixture", "fixtures", "stub", "stubs", "fake", "fakes",
];
const MOCK_DIRS: [&str; 10] = [
    "__fixtures__",
    "__mocks__",
    "__snapshots__",
    "fakes",
    "fixture",
    "fixtures",
    "mock",
    "mocks",
    "stubs",
    "testdata",
];
const TEST_DIRS: [&str; 8] = [
    "__tests__",
    "__test__",
    "cypress",
    "e2e",
    "spec",
    "specs",
    "test",
    "tests",
];
const CLASS_TEST_EXTS: [&str; 9] = [
    "java", "kt", "kts", "scala", "groovy", "cs", "php", "swift", "m",
];
const DOCS_EXTS: [&str; 6] = ["md", "mdx", "markdown", "rst", "adoc", "txt"];
const CONFIG_EXTS: [&str; 12] = [
    "json",
    "jsonc",
    "json5",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "plist",
    "xcconfig",
];

/// `name` split at its last dot into a stem and a non-empty extension.
fn stem_ext(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('.').filter(|(_, ext)| !ext.is_empty())
}

fn generated_name(name: &str) -> bool {
    name.contains(".generated.")
        || name.contains(".gen.")
        || GENERATED_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// `\.snap$`, `\.(mocks?|fixtures?|stubs?|fakes?)\.[^.]+$` and
/// `^(mocks?|...)([-_.A-Z]|$)`, case-sensitive.
fn mock_name(name: &str) -> bool {
    if name.ends_with(".snap") {
        return true;
    }
    if let Some((stem, _)) = stem_ext(name) {
        if let Some((_, word)) = stem.rsplit_once('.') {
            if MOCK_WORDS.contains(&word) {
                return true;
            }
        }
    }
    MOCK_WORDS.iter().any(|word| {
        name.strip_prefix(word).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|c| matches!(c, '-' | '_' | '.') || c.is_ascii_uppercase())
        })
    })
}

/// T3's case-insensitive `TEST_NAME` and case-sensitive `CLASS_TEST_NAME`.
fn test_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if let Some((stem, _)) = stem_ext(&lower) {
        if stem
            .rsplit_once('.')
            .is_some_and(|(_, word)| matches!(word, "test" | "spec" | "e2e"))
        {
            return true;
        }
        if stem.ends_with("_test") || stem.ends_with("_spec") {
            return true;
        }
        if stem == "vitest.setup" || stem == "jest.setup" {
            return true;
        }
    }
    if lower.starts_with("test_") && lower.ends_with(".py") && lower.len() > "test_.py".len() {
        return true;
    }
    if lower == "conftest.py" {
        return true;
    }
    stem_ext(name).is_some_and(|(stem, ext)| {
        CLASS_TEST_EXTS.contains(&ext)
            && (stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("Spec"))
    })
}

/// Plain-text files tools read as input, not prose (T3's `NOT_DOCS_NAME`).
fn tool_text_name(lower: &str) -> bool {
    lower.strip_suffix(".txt").is_some_and(|stem| {
        stem.starts_with("requirements")
            || stem.starts_with("constraints")
            || matches!(stem, "cmakelists" | "robots" | "llms")
    })
}

/// Sorts a changed file by its path alone, using common naming conventions:
/// lockfiles and build output are generated, tests and their support
/// (mocks, fixtures, snapshots) are tests, prose is docs, data and tool
/// configuration is config, and the rest is code.
pub fn classify_path(path: &str) -> FileKind {
    let normalized = path.replace('\\', "/");
    let mut segments: Vec<&str> = normalized.split('/').collect();
    let name = segments.pop().unwrap_or("");
    let dirs: Vec<String> = segments.iter().map(|dir| dir.to_lowercase()).collect();
    let in_dir = |set: &[&str]| dirs.iter().any(|dir| set.contains(&dir.as_str()));
    let lower = name.to_lowercase();
    if GENERATED_FILES.contains(&lower.as_str()) || generated_name(name) || in_dir(&GENERATED_DIRS)
    {
        return FileKind::Generated;
    }
    if mock_name(name) || in_dir(&MOCK_DIRS) {
        return FileKind::Test;
    }
    if test_name(name) || in_dir(&TEST_DIRS) {
        return FileKind::Test;
    }
    let ext = stem_ext(&lower).map(|(_, ext)| ext);
    if ext.is_some_and(|ext| DOCS_EXTS.contains(&ext)) && !tool_text_name(&lower) {
        return FileKind::Docs;
    }
    if ext.is_some_and(|ext| CONFIG_EXTS.contains(&ext))
        || lower.starts_with('.')
        || tool_text_name(&lower)
    {
        return FileKind::Config;
    }
    FileKind::Code
}

/// Tree order: at each level folders come before files, then by name.
pub fn tree_order(a: &str, b: &str) -> Ordering {
    let mut left = a.split('/').peekable();
    let mut right = b.split('/').peekable();
    loop {
        match (left.next(), right.next()) {
            (Some(x), Some(y)) => {
                let x_file = left.peek().is_none();
                let y_file = right.peek().is_none();
                if x_file != y_file {
                    return if x_file {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    };
                }
                match x.cmp(y) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Which view a file opens in: Uncommitted when it has uncommitted
/// changes (or there is no base to compare with), else vs base.
pub fn edit_review_source(uncommitted: bool, has_base: bool) -> DiffSource {
    if uncommitted || !has_base {
        DiffSource::Uncommitted
    } else {
        DiffSource::VsBase
    }
}

/// What an `open_review` resolves to: the pane's target and source, and
/// the checkout-relative file to scroll to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReview {
    pub target: ReviewTarget,
    pub source: DiffSource,
    pub focus: String,
}

/// A captured turn's immutable snapshot endpoints. Available without runtime IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTurn {
    pub number: u64,
    pub start: Option<String>,
    pub end: Option<String>,
    pub timestamp: i64,
}

/// A collected review plus the metadata needed by the review authority.
#[derive(Debug, Clone)]
pub struct ReviewLoad {
    pub diff: ReviewDiff,
    pub commits: Vec<ReviewCommit>,
    pub turns: Vec<ReviewTurn>,
    pub commits_truncated: bool,
    pub commits_error: Option<String>,
    pub canonical_checkout: PathBuf,
    /// None means listing failed or was incomplete; do not prune viewed marks.
    pub live_paths: Option<std::collections::BTreeSet<String>>,
}
