//! Review pane (TRU-178): what the work changed in one checkout, read as a
//! structured diff and rendered as one page in the Viewer webview.
//!
//! A Workspace overview branch row opens it. The diff comes from the same
//! git calls the overview's branch diff used (`git diff HEAD` for
//! Uncommitted, `git diff <base>...HEAD` for vs base, `ls-files --others`
//! for untracked files, `merge-base` for the base commit), read as a file
//! list first (`--raw --numstat -z`) and then as a patch for the files that
//! fit the caps. Everything here is free of iced and wry: main.rs runs
//! [`load_review`] off the UI thread, renders [`render_page`] into the
//! Viewer surface and feeds the page's messages through [`parse_ipc`].
//!
//! Caps, so a huge diff can never hang the app:
//!
//! - a file shows at most [`FILE_LINE_CAP`] lines or [`FILE_BYTE_CAP`]
//!   bytes of text and is marked `truncated` past that (untracked files are
//!   read only that far);
//! - files are admitted in kind order (code, config, docs, tests,
//!   generated) while their changed lines fit [`ADMIT_LINE_BUDGET`]; the
//!   rest are listed with their counts but collapsed as "too large to show
//!   here" (`omitted`);
//! - rendering stops for good at [`TOTAL_LINE_CAP`] lines or
//!   [`TOTAL_BYTE_CAP`] bytes, context included;
//! - at most [`FILE_LIST_CAP`] files are listed; the rest are counted.
//!
//! Viewed marks live in `<shared config>/review-viewed.json`
//! ([`ViewedMarks`]), keyed by canonical checkout and file path, holding
//! the content hash of the version viewed.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace_overview::{git_text, run_git_records, BaseChoice, DiffSource, GIT_TIMEOUT};

/// Lines of one file shown before it is cut (`truncated`).
pub const FILE_LINE_CAP: usize = 2_000;
/// Bytes of one file's text shown before it is cut (`truncated`).
pub const FILE_BYTE_CAP: usize = 256 * 1024;
/// Changed lines admitted for display across the whole diff, each file
/// costing at most [`FILE_LINE_CAP`]. Files that do not fit are `omitted`.
pub const ADMIT_LINE_BUDGET: usize = 20_000;
/// Hard stop on rendered lines (context included) across the diff.
pub const TOTAL_LINE_CAP: usize = 30_000;
/// Hard stop on rendered text bytes across the diff.
pub const TOTAL_BYTE_CAP: usize = 4 * 1024 * 1024;
/// Files listed in the pane; the rest are only counted (`unlisted`).
pub const FILE_LIST_CAP: usize = 2_000;
/// Untracked names read; past this the list is incomplete and viewed marks
/// are not pruned.
pub const UNTRACKED_SCAN_CAP: usize = 10_000;
/// Bytes sniffed for a NUL to call a file binary (git's heuristic).
const BINARY_SNIFF_BYTES: usize = 8_000;
/// Paths per `git hash-object` call.
const HASH_CHUNK: usize = 200;
/// The shared-config file holding viewed marks.
pub const VIEWED_FILE: &str = "review-viewed.json";

// ---------------------------------------------------------------------------
// The structured diff.

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

    fn index(self) -> usize {
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
    /// Cut at [`FILE_LINE_CAP`] / [`FILE_BYTE_CAP`] or the total caps.
    pub truncated: bool,
    /// Listed but not shown: past [`ADMIT_LINE_BUDGET`] or the total caps.
    pub omitted: bool,
    /// Why a file has no lines when that is not binary or omitted (a nested
    /// repository, a file that could not be read).
    pub note: Option<String>,
    pub hunks: Vec<ReviewHunk>,
    /// The version shown, for viewed marks: `git hash-object` of the
    /// working-tree file (Uncommitted) or its blob id (vs base);
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
    /// Files in the diff past [`FILE_LIST_CAP`], not listed.
    pub unlisted: usize,
}

impl ReviewDiff {
    pub fn file(&self, path: &str) -> Option<&ReviewFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

/// A loaded review: the diff plus what viewed marks need.
#[derive(Debug, Clone)]
pub struct ReviewLoad {
    pub diff: ReviewDiff,
    /// The marks' key for this checkout.
    pub canonical_checkout: PathBuf,
    /// Every path in any diff of the checkout (both sources), or `None` when
    /// a listing was incomplete or failed: marks are then not pruned.
    pub live_paths: Option<BTreeSet<String>>,
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

// ---------------------------------------------------------------------------
// Parsing git output.

/// One file of `git diff --raw --numstat -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryEntry {
    pub path: String,
    pub old_path: Option<String>,
    /// The raw status letter: `A`, `M`, `D`, `R`, `C`, `T`, `U`, `X`.
    pub status: char,
    pub old_mode: String,
    pub new_mode: String,
    pub old_blob: String,
    pub new_blob: String,
    pub additions: usize,
    pub deletions: usize,
    /// numstat reported `-` counts.
    pub binary: bool,
}

impl SummaryEntry {
    fn is_submodule(&self) -> bool {
        self.new_mode == "160000" || self.old_mode == "160000"
    }
}

/// Parses `git diff --raw --numstat -z` output: every raw record first,
/// then a numstat record per file.
pub fn parse_raw_numstat(bytes: &[u8]) -> Result<Vec<SummaryEntry>, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut tokens: Vec<&str> = text.split('\0').collect();
    if tokens.last() == Some(&"") {
        tokens.pop();
    }
    let mut entries: Vec<SummaryEntry> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut counted = 0usize;
    let mut tokens = tokens.into_iter();
    fn next_path<'a>(
        tokens: &mut impl Iterator<Item = &'a str>,
        what: &str,
    ) -> Result<String, String> {
        tokens
            .next()
            .map(str::to_string)
            .ok_or_else(|| format!("git diff --raw: {what} missing"))
    }
    while let Some(token) = tokens.next() {
        if let Some(raw) = token.strip_prefix(':') {
            let fields: Vec<&str> = raw.split(' ').collect();
            let [old_mode, new_mode, old_blob, new_blob, status] = fields[..] else {
                return Err(format!("git diff --raw: unexpected record {token:?}"));
            };
            let letter = status
                .chars()
                .next()
                .ok_or_else(|| format!("git diff --raw: no status in {token:?}"))?;
            let (old_path, path) = if matches!(letter, 'R' | 'C') {
                let old = next_path(&mut tokens, "rename source")?;
                (Some(old), next_path(&mut tokens, "rename target")?)
            } else {
                (None, next_path(&mut tokens, "path")?)
            };
            by_path.insert(path.clone(), entries.len());
            entries.push(SummaryEntry {
                path,
                old_path,
                status: letter,
                old_mode: old_mode.to_string(),
                new_mode: new_mode.to_string(),
                old_blob: old_blob.to_string(),
                new_blob: new_blob.to_string(),
                additions: 0,
                deletions: 0,
                binary: false,
            });
        } else {
            let mut fields = token.splitn(3, '\t');
            let (Some(added), Some(deleted), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("git diff --numstat: unexpected record {token:?}"));
            };
            let path = if path.is_empty() {
                next_path(&mut tokens, "numstat rename source")?;
                next_path(&mut tokens, "numstat rename target")?
            } else {
                path.to_string()
            };
            let index = *by_path
                .get(&path)
                .ok_or_else(|| format!("git diff --numstat: {path} has no raw record"))?;
            let entry = &mut entries[index];
            if added == "-" || deleted == "-" {
                entry.binary = true;
            } else {
                let count = |field: &str| {
                    field
                        .parse::<usize>()
                        .map_err(|_| format!("git diff --numstat: bad count {field:?} for {path}"))
                };
                entry.additions = count(added)?;
                entry.deletions = count(deleted)?;
            }
            counted += 1;
        }
    }
    if counted != entries.len() {
        return Err(format!(
            "git diff --numstat counted {counted} of {} files",
            entries.len()
        ));
    }
    Ok(entries)
}

/// `prefix` + `path` as git writes it in a `diff --git` header with
/// `core.quotePath=false`: C-quoted only when it holds a quote, a backslash
/// or a control character.
pub fn quote_git_path(prefix: &str, path: &str) -> String {
    let full = format!("{prefix}{path}");
    let needs = full
        .bytes()
        .any(|b| b < 0x20 || b == b'"' || b == b'\\' || b == 0x7f);
    if !needs {
        return full;
    }
    let mut out = String::from("\"");
    for c in full.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `diff --git` line git writes for `entry`.
fn expected_header(entry: &SummaryEntry) -> String {
    let old = entry.old_path.as_deref().unwrap_or(&entry.path);
    format!(
        "diff --git {} {}",
        quote_git_path("a/", old),
        quote_git_path("b/", &entry.path)
    )
}

/// One file's patch as read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchFile {
    /// The `diff --git ...` line.
    pub header: String,
    pub hunks: Vec<ReviewHunk>,
    pub binary: bool,
    pub truncated: bool,
    lines: usize,
    bytes: usize,
}

/// Running totals shared by the patch and the untracked files.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub lines: usize,
    pub bytes: usize,
}

impl Budget {
    fn exhausted(&self) -> bool {
        self.lines >= TOTAL_LINE_CAP || self.bytes >= TOTAL_BYTE_CAP
    }
}

/// Where a line goes: stored, cut short, or dropped.
enum Admit {
    Whole,
    Cut(usize),
    Drop,
}

/// What fits of a `len`-byte line into a file holding `lines` / `bytes`
/// under the per-file and total caps.
fn admit_line(lines: usize, bytes: usize, len: usize, total: &Budget) -> Admit {
    if lines >= FILE_LINE_CAP || total.exhausted() {
        return Admit::Drop;
    }
    let room = (FILE_BYTE_CAP - bytes.min(FILE_BYTE_CAP)).min(TOTAL_BYTE_CAP - total.bytes);
    if len <= room {
        Admit::Whole
    } else if room > 0 {
        Admit::Cut(room)
    } else {
        Admit::Drop
    }
}

/// `text` cut to at most `max` bytes on a char boundary.
fn cut_to(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Parses unified `git diff` output into per-file hunks, honouring the
/// caps. Hunk line counts decide what is content, so an added line reading
/// `++ x` is never taken for a `+++` header.
#[derive(Debug, Default)]
pub struct PatchParser {
    pub files: Vec<PatchFile>,
    pub budget: Budget,
    /// The total caps were reached: reading should stop.
    pub stopped: bool,
    old_left: u32,
    new_left: u32,
    old_no: u32,
    new_no: u32,
}

impl PatchParser {
    /// Feeds one line (without its newline). Returns false once the total
    /// caps are reached.
    pub fn feed(&mut self, line: &str) -> bool {
        if self.stopped {
            return false;
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        if self.old_left > 0 || self.new_left > 0 {
            let (op, text) = match line.as_bytes().first() {
                Some(b' ') => (LineOp::Context, &line[1..]),
                Some(b'+') => (LineOp::Add, &line[1..]),
                Some(b'-') => (LineOp::Del, &line[1..]),
                Some(b'\\') => return true,
                // A context line whose leading space was stripped.
                None => (LineOp::Context, ""),
                Some(_) => {
                    self.old_left = 0;
                    self.new_left = 0;
                    self.header(line);
                    return !self.stopped;
                }
            };
            let (old_no, new_no) = match op {
                LineOp::Context => {
                    let numbers = (Some(self.old_no), Some(self.new_no));
                    self.old_no += 1;
                    self.new_no += 1;
                    self.old_left = self.old_left.saturating_sub(1);
                    self.new_left = self.new_left.saturating_sub(1);
                    numbers
                }
                LineOp::Add => {
                    let numbers = (None, Some(self.new_no));
                    self.new_no += 1;
                    self.new_left = self.new_left.saturating_sub(1);
                    numbers
                }
                LineOp::Del => {
                    let numbers = (Some(self.old_no), None);
                    self.old_no += 1;
                    self.old_left = self.old_left.saturating_sub(1);
                    numbers
                }
            };
            self.push(ReviewLine {
                op,
                old_no,
                new_no,
                text: text.to_string(),
            });
            return !self.stopped;
        }
        self.header(line);
        !self.stopped
    }

    fn header(&mut self, line: &str) {
        if line.starts_with("diff --git ") {
            self.files.push(PatchFile {
                header: line.to_string(),
                ..PatchFile::default()
            });
        } else if line.starts_with("@@ ") {
            let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line) else {
                return;
            };
            self.old_no = old_start;
            self.new_no = new_start;
            self.old_left = old_count;
            self.new_left = new_count;
            if let Some(file) = self.files.last_mut() {
                if !file.truncated {
                    file.hunks.push(ReviewHunk {
                        header: line.to_string(),
                        lines: Vec::new(),
                    });
                }
            }
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            if let Some(file) = self.files.last_mut() {
                file.binary = true;
            }
        }
    }

    fn push(&mut self, mut line: ReviewLine) {
        let Some(file) = self.files.last_mut() else {
            return;
        };
        if file.truncated {
            return;
        }
        match admit_line(file.lines, file.bytes, line.text.len(), &self.budget) {
            Admit::Drop => {
                file.truncated = true;
                if self.budget.exhausted() {
                    self.stopped = true;
                }
                return;
            }
            Admit::Cut(room) => {
                line.text = cut_to(&line.text, room).to_string();
                file.truncated = true;
            }
            Admit::Whole => {}
        }
        file.lines += 1;
        file.bytes += line.text.len();
        self.budget.lines += 1;
        self.budget.bytes += line.text.len();
        if let Some(hunk) = file.hunks.last_mut() {
            hunk.lines.push(line);
        }
        if self.budget.exhausted() {
            // Reading stops here, so whatever follows in this file is unseen.
            self.stopped = true;
            file.truncated = true;
        }
    }
}

/// `@@ -12,5 +14,7 @@ ...` -> (12, 5, 14, 7); a missing count is 1.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = line.strip_prefix("@@ ")?.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

// ---------------------------------------------------------------------------
// Untracked files.

/// A first look at an untracked path: its size and whether it is binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sniff {
    Text {
        size: u64,
    },
    Binary,
    /// Not a regular file (a nested repository) or unreadable.
    Unreadable(String),
}

pub fn sniff_untracked(path: &Path) -> Sniff {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) => return Sniff::Unreadable(format!("cannot read: {error}")),
    };
    if meta.is_dir() {
        return Sniff::Unreadable("a folder (nested repository); not shown".to_string());
    }
    let mut head = Vec::with_capacity(BINARY_SNIFF_BYTES);
    let read = std::fs::File::open(path)
        .and_then(|file| file.take(BINARY_SNIFF_BYTES as u64).read_to_end(&mut head));
    match read {
        Ok(_) if head.contains(&0) => Sniff::Binary,
        Ok(_) => Sniff::Text { size: meta.len() },
        Err(error) => Sniff::Unreadable(format!("cannot read: {error}")),
    }
}

/// An untracked file's hunks, the number of lines shown and whether it was
/// cut.
pub type AddedLines = (Vec<ReviewHunk>, usize, bool);

/// An untracked text file as added lines, cut at the caps.
pub fn untracked_hunks(bytes: &[u8], more_on_disk: bool, budget: &mut Budget) -> AddedLines {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = Vec::new();
    let mut truncated = more_on_disk;
    let mut file_bytes = 0usize;
    let body = text.strip_suffix('\n').unwrap_or(&text);
    if !text.is_empty() {
        for (index, raw) in body.split('\n').enumerate() {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let text = match admit_line(lines.len(), file_bytes, raw.len(), budget) {
                Admit::Drop => {
                    truncated = true;
                    break;
                }
                Admit::Cut(room) => {
                    truncated = true;
                    cut_to(raw, room)
                }
                Admit::Whole => raw,
            };
            file_bytes += text.len();
            budget.lines += 1;
            budget.bytes += text.len();
            lines.push(ReviewLine {
                op: LineOp::Add,
                old_no: None,
                new_no: Some(index as u32 + 1),
                text: text.to_string(),
            });
            if text.len() < raw.len() {
                break;
            }
        }
    }
    let shown = lines.len();
    if lines.is_empty() {
        return (Vec::new(), 0, truncated);
    }
    let hunk = ReviewHunk {
        header: format!("@@ -0,0 +1,{shown} @@"),
        lines,
    };
    (vec![hunk], shown, truncated)
}

fn read_untracked(path: &Path, budget: &mut Budget) -> Result<AddedLines, String> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|e| format!("cannot read: {e}"))?;
    file.take(FILE_BYTE_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read: {e}"))?;
    let more = bytes.len() > FILE_BYTE_CAP;
    bytes.truncate(FILE_BYTE_CAP);
    Ok(untracked_hunks(&bytes, more, budget))
}

// ---------------------------------------------------------------------------
// Ordering and admission.

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

/// Which files are shown: by kind order, then the order given, each costing
/// its changed lines up to [`FILE_LINE_CAP`], while they fit `budget`. A
/// file that does not fit is skipped and smaller ones after it may still
/// fit. Returns the admitted indices.
pub fn admit(costs: &[(FileKind, usize)], budget: usize) -> HashSet<usize> {
    let mut order: Vec<usize> = (0..costs.len()).collect();
    order.sort_by_key(|&index| costs[index].0.index());
    let mut left = budget;
    let mut admitted = HashSet::new();
    for index in order {
        let cost = costs[index].1.min(FILE_LINE_CAP);
        if cost <= left {
            left -= cost;
            admitted.insert(index);
        }
    }
    admitted
}

// ---------------------------------------------------------------------------
// Loading.

/// What the loader knows about one file before reading content.
#[derive(Debug, Clone)]
enum Pending {
    Tracked(SummaryEntry),
    Untracked { path: String, sniff: Option<Sniff> },
}

impl Pending {
    fn path(&self) -> &str {
        match self {
            Pending::Tracked(entry) => &entry.path,
            Pending::Untracked { path, .. } => path,
        }
    }
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

async fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    run_git_records(dir, args, GIT_TIMEOUT, 0, |record| {
        out.extend_from_slice(record);
        out.push(0);
        Ok(true)
    })
    .await?;
    Ok(out)
}

/// Untracked names, at most `cap`; the flag says the list is complete.
async fn untracked_names(dir: &Path, cap: usize) -> Result<(Vec<String>, bool), String> {
    let mut names = Vec::new();
    let mut complete = true;
    run_git_records(
        dir,
        &[
            "--no-optional-locks",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        GIT_TIMEOUT,
        0,
        |record| {
            if names.len() >= cap {
                complete = false;
                return Ok(false);
            }
            names.push(String::from_utf8_lossy(record).into_owned());
            Ok(true)
        },
    )
    .await?;
    Ok((names, complete))
}

/// `git hash-object` of each path, in chunks. A chunk that fails (a file
/// vanished between calls) is retried path by path; a path that still fails
/// gets `None`, logged.
async fn hash_objects(dir: &Path, paths: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in paths.chunks(HASH_CHUNK) {
        let mut args = vec!["--no-optional-locks", "hash-object", "--"];
        args.extend(chunk.iter().map(String::as_str));
        match git_text(dir, &args, GIT_TIMEOUT).await {
            Ok(text) => {
                let ids: Vec<&str> = text.lines().collect();
                if ids.len() == chunk.len() {
                    for (path, id) in chunk.iter().zip(ids) {
                        out.insert(path.clone(), id.to_string());
                    }
                    continue;
                }
                eprintln!(
                    "[review] git hash-object gave {} ids for {} paths in {}; hashing one by one",
                    ids.len(),
                    chunk.len(),
                    dir.display()
                );
            }
            Err(error) => {
                eprintln!("[review] {error}; hashing one by one");
            }
        }
        for path in chunk {
            match git_text(
                dir,
                &["--no-optional-locks", "hash-object", "--", path.as_str()],
                GIT_TIMEOUT,
            )
            .await
            {
                Ok(text) => {
                    out.insert(path.clone(), text.trim().to_string());
                }
                Err(error) => eprintln!("[review] cannot hash {path}: {error}"),
            }
        }
    }
    out
}

fn tracked_status(entry: &SummaryEntry) -> FileStatus {
    let status = match entry.status {
        'A' | 'C' => FileStatus::Added,
        'D' => FileStatus::Deleted,
        'R' => FileStatus::Renamed,
        _ => FileStatus::Modified,
    };
    if entry.binary && status != FileStatus::Deleted {
        FileStatus::Binary
    } else {
        status
    }
}

/// Loads the review of `target` for `source`. Never call on the UI thread.
pub async fn load_review(target: ReviewTarget, source: DiffSource) -> Result<ReviewLoad, String> {
    let checkout = target.checkout.clone();
    let canonical_checkout = tokio::fs::canonicalize(&checkout)
        .await
        .map_err(|error| format!("cannot resolve {}: {error}", checkout.display()))?;
    let base = match source {
        DiffSource::Uncommitted => None,
        DiffSource::VsBase => Some(target.base.clone().ok_or_else(|| {
            format!(
                "no base branch found for {}: no task base, tracked branch, or develop/main/master/trunk",
                checkout.display()
            )
        })?),
    };
    let range = match &base {
        None => "HEAD".to_string(),
        Some(base) => format!("{}...HEAD", base.reference),
    };

    // The file list.
    let summary = git_bytes(
        &checkout,
        &[
            "--no-optional-locks",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--raw",
            "--numstat",
            "--no-abbrev",
            "-z",
            &range,
            "--",
        ],
    )
    .await?;
    let summary = parse_raw_numstat(&summary)?;
    let (untracked, untracked_complete) = match source {
        DiffSource::Uncommitted => untracked_names(&checkout, UNTRACKED_SCAN_CAP).await?,
        DiffSource::VsBase => (Vec::new(), true),
    };
    let mut all_paths: BTreeSet<String> = summary.iter().map(|e| e.path.clone()).collect();
    all_paths.extend(untracked.iter().cloned());
    let mut pending: Vec<Pending> = summary.into_iter().map(Pending::Tracked).collect();
    pending.extend(
        untracked
            .into_iter()
            .map(|path| Pending::Untracked { path, sniff: None }),
    );
    pending.sort_by(|a, b| tree_order(a.path(), b.path()));
    let unlisted = pending.len().saturating_sub(FILE_LIST_CAP);
    pending.truncate(FILE_LIST_CAP);

    // A first look at untracked files: binary, size.
    let sniff_dir = checkout.clone();
    let pending = tokio::task::spawn_blocking(move || {
        for item in &mut pending {
            if let Pending::Untracked { path, sniff } = item {
                *sniff = Some(sniff_untracked(&sniff_dir.join(&*path)));
            }
        }
        pending
    })
    .await
    .map_err(|error| format!("reading untracked files failed: {error}"))?;

    // What is shown.
    let costs: Vec<(FileKind, usize)> = pending
        .iter()
        .map(|item| {
            let kind = classify_path(item.path());
            let cost = match item {
                Pending::Tracked(entry) if entry.binary => 0,
                Pending::Tracked(entry) => entry.additions + entry.deletions,
                Pending::Untracked {
                    sniff: Some(Sniff::Text { size }),
                    ..
                } => (*size as usize / 32) + 1,
                Pending::Untracked { .. } => 0,
            };
            (kind, cost)
        })
        .collect();
    let admitted = admit(&costs, ADMIT_LINE_BUDGET);

    // The patch, for admitted tracked files only.
    let mut pathspecs: Vec<String> = Vec::new();
    let mut headers: HashMap<String, usize> = HashMap::new();
    for (index, item) in pending.iter().enumerate() {
        if let Pending::Tracked(entry) = item {
            if admitted.contains(&index) && !entry.binary && entry.additions + entry.deletions > 0 {
                headers.insert(expected_header(entry), index);
                if let Some(old) = &entry.old_path {
                    pathspecs.push(old.clone());
                }
                pathspecs.push(entry.path.clone());
            }
        }
    }
    let mut parser = PatchParser::default();
    if !pathspecs.is_empty() {
        let mut args = vec![
            "-c",
            "core.quotePath=false",
            "--no-optional-locks",
            "--literal-pathspecs",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            &range,
            "--",
        ];
        args.extend(pathspecs.iter().map(String::as_str));
        run_git_records(&checkout, &args, GIT_TIMEOUT, b'\n', |record| {
            Ok(parser.feed(&String::from_utf8_lossy(record)))
        })
        .await?;
    }
    let mut patches: HashMap<usize, PatchFile> = HashMap::new();
    for patch in std::mem::take(&mut parser.files) {
        match headers.get(&patch.header) {
            Some(&index) => {
                patches.insert(index, patch);
            }
            None => eprintln!(
                "[review] {}: patch {:?} matches no listed file",
                checkout.display(),
                patch.header
            ),
        }
    }

    // Untracked content, then the files.
    let read_dir = checkout.clone();
    let mut budget = parser.budget;
    let admitted_for_read = admitted.clone();
    let untracked_content = tokio::task::spawn_blocking(move || {
        let mut out: HashMap<usize, Result<AddedLines, String>> = HashMap::new();
        for (index, item) in pending.iter().enumerate() {
            if let Pending::Untracked {
                path,
                sniff: Some(Sniff::Text { .. }),
            } = item
            {
                if admitted_for_read.contains(&index) && !budget.exhausted() {
                    out.insert(index, read_untracked(&read_dir.join(path), &mut budget));
                }
            }
        }
        (pending, out)
    })
    .await
    .map_err(|error| format!("reading untracked files failed: {error}"))?;
    let (pending, mut untracked_content) = untracked_content;

    let mut files = Vec::with_capacity(pending.len());
    let mut to_hash: Vec<String> = Vec::new();
    for (index, item) in pending.into_iter().enumerate() {
        let kind = costs[index].0;
        let shown = admitted.contains(&index);
        match item {
            Pending::Tracked(entry) => {
                let status = tracked_status(&entry);
                let wants_lines = !entry.binary && entry.additions + entry.deletions > 0;
                let patch = patches.remove(&index);
                let omitted = wants_lines && (!shown || patch.is_none());
                let (hunks, truncated) = match patch {
                    Some(patch) => (patch.hunks, patch.truncated),
                    None => (Vec::new(), false),
                };
                let content_id = if entry.is_submodule() {
                    Some(format!("commit:{}", entry.new_blob))
                } else if status == FileStatus::Deleted {
                    Some(format!("deleted:{}", entry.old_blob))
                } else if source == DiffSource::VsBase {
                    Some(entry.new_blob.clone())
                } else {
                    to_hash.push(entry.path.clone());
                    None
                };
                files.push(ReviewFile {
                    path: entry.path,
                    old_path: entry.old_path.filter(|_| entry.status == 'R'),
                    status,
                    kind,
                    additions: entry.additions,
                    deletions: entry.deletions,
                    truncated,
                    omitted,
                    note: None,
                    hunks,
                    content_id,
                });
            }
            Pending::Untracked { path, sniff } => {
                let mut file = ReviewFile {
                    path,
                    old_path: None,
                    status: FileStatus::Untracked,
                    kind,
                    additions: 0,
                    deletions: 0,
                    truncated: false,
                    omitted: false,
                    note: None,
                    hunks: Vec::new(),
                    content_id: None,
                };
                match sniff {
                    Some(Sniff::Binary) => {
                        file.status = FileStatus::Binary;
                        to_hash.push(file.path.clone());
                    }
                    Some(Sniff::Unreadable(why)) => file.note = Some(why),
                    Some(Sniff::Text { .. }) => {
                        to_hash.push(file.path.clone());
                        match untracked_content.remove(&index) {
                            Some(Ok((hunks, lines, truncated))) => {
                                file.hunks = hunks;
                                file.additions = lines;
                                file.truncated = truncated;
                            }
                            Some(Err(why)) => file.note = Some(why),
                            None => file.omitted = true,
                        }
                    }
                    None => file.note = Some("not read".to_string()),
                }
                files.push(file);
            }
        }
    }
    if !to_hash.is_empty() {
        let ids = hash_objects(&checkout, &to_hash).await;
        for file in &mut files {
            if file.content_id.is_none() {
                file.content_id = ids.get(&file.path).cloned();
            }
        }
    }

    let merge_base = match &base {
        Some(base) => {
            let text = git_text(
                &checkout,
                &["--no-optional-locks", "merge-base", &base.reference, "HEAD"],
                GIT_TIMEOUT,
            )
            .await?;
            text.trim().get(..7).map(str::to_string)
        }
        None => None,
    };

    // Every path in the other source, so marks for files still in either
    // diff survive pruning.
    let live_paths = if !untracked_complete {
        None
    } else {
        match other_source_paths(&checkout, source, target.base.as_ref()).await {
            Ok(other) => {
                all_paths.extend(other);
                Some(all_paths)
            }
            Err(error) => {
                eprintln!("[review] not pruning viewed marks: {error}");
                None
            }
        }
    };

    let totals = ReviewTotals::of(&files);
    Ok(ReviewLoad {
        diff: ReviewDiff {
            checkout,
            branch: target.branch,
            base: target.base,
            source,
            merge_base,
            files,
            totals,
            unlisted,
        },
        canonical_checkout,
        live_paths,
    })
}

async fn other_source_paths(
    checkout: &Path,
    source: DiffSource,
    base: Option<&BaseChoice>,
) -> Result<Vec<String>, String> {
    match source {
        DiffSource::Uncommitted => {
            let Some(base) = base else {
                return Ok(Vec::new());
            };
            let range = format!("{}...HEAD", base.reference);
            let names = git_bytes(
                checkout,
                &[
                    "--no-optional-locks",
                    "diff",
                    "--no-ext-diff",
                    "-M",
                    "--name-only",
                    "-z",
                    &range,
                    "--",
                ],
            )
            .await?;
            Ok(split_nul(&names))
        }
        DiffSource::VsBase => {
            let names = git_bytes(
                checkout,
                &[
                    "--no-optional-locks",
                    "diff",
                    "--no-ext-diff",
                    "-M",
                    "--name-only",
                    "-z",
                    "HEAD",
                    "--",
                ],
            )
            .await?;
            let mut out = split_nul(&names);
            let (untracked, complete) = untracked_names(checkout, UNTRACKED_SCAN_CAP).await?;
            if !complete {
                return Err(format!(
                    "more than {UNTRACKED_SCAN_CAP} untracked files in {}",
                    checkout.display()
                ));
            }
            out.extend(untracked);
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Viewed marks.

/// Whether the version shown was marked viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewedState {
    Unviewed,
    Viewed,
    /// Marked viewed, but the content changed since: shown expanded.
    ChangedSinceViewed,
}

/// `review-viewed.json`: canonical checkout -> file path -> content id of
/// the version viewed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewedMarks {
    #[serde(default)]
    pub checkouts: BTreeMap<String, BTreeMap<String, String>>,
}

fn checkout_key(checkout: &Path) -> String {
    checkout.to_string_lossy().into_owned()
}

impl ViewedMarks {
    /// Reads the marks from `dir`; none when the file is absent. A file that
    /// cannot be read or parsed is an error, so it is never overwritten.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(VIEWED_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| format!("cannot parse {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode viewed marks: {error}"))?;
        crate::merge_plan::write_atomically(&dir.join(VIEWED_FILE), &json)
    }

    pub fn state(&self, checkout: &Path, file: &ReviewFile) -> ViewedState {
        let Some(marked) = self
            .checkouts
            .get(&checkout_key(checkout))
            .and_then(|files| files.get(&file.path))
        else {
            return ViewedState::Unviewed;
        };
        if file.content_id.as_deref() == Some(marked.as_str()) {
            ViewedState::Viewed
        } else {
            ViewedState::ChangedSinceViewed
        }
    }

    /// Marks `file` viewed at its current content, or clears its mark.
    /// Returns false when a file without a content id is marked: there is
    /// nothing to compare a later version with.
    pub fn set(&mut self, checkout: &Path, file: &ReviewFile, viewed: bool) -> bool {
        let key = checkout_key(checkout);
        if !viewed {
            if let Some(files) = self.checkouts.get_mut(&key) {
                files.remove(&file.path);
                if files.is_empty() {
                    self.checkouts.remove(&key);
                }
            }
            return true;
        }
        let Some(id) = &file.content_id else {
            return false;
        };
        self.checkouts
            .entry(key)
            .or_default()
            .insert(file.path.clone(), id.clone());
        true
    }

    /// Drops the checkout's marks for files in no diff of it. Returns
    /// whether anything changed.
    pub fn prune(&mut self, checkout: &Path, live: &BTreeSet<String>) -> bool {
        let key = checkout_key(checkout);
        let Some(files) = self.checkouts.get_mut(&key) else {
            return false;
        };
        let before = files.len();
        files.retain(|path, _| live.contains(path));
        let changed = files.len() != before;
        if files.is_empty() {
            self.checkouts.remove(&key);
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// IPC from the page.

/// A message the review page posts with `window.ipc.postMessage`. Every
/// message carries the page's `token`; main.rs drops messages whose token
/// is not the open pane's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewIpc {
    /// The Uncommitted / vs base switch; `hide_noise` is the toggle's state
    /// to keep across the reload.
    SetSource {
        token: String,
        source: DiffSource,
        hide_noise: bool,
    },
    /// A file's Viewed checkbox.
    SetViewed {
        token: String,
        path: String,
        viewed: bool,
    },
    Close {
        token: String,
    },
    /// Open a listed file in GitTerm's file viewer.
    OpenFile {
        token: String,
        path: String,
    },
}

impl ReviewIpc {
    pub fn token(&self) -> &str {
        match self {
            ReviewIpc::SetSource { token, .. }
            | ReviewIpc::SetViewed { token, .. }
            | ReviewIpc::Close { token }
            | ReviewIpc::OpenFile { token, .. } => token,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WireIpc {
    #[serde(rename = "review.source")]
    Source {
        token: String,
        source: String,
        hide_noise: bool,
    },
    #[serde(rename = "review.viewed")]
    Viewed {
        token: String,
        path: String,
        viewed: bool,
    },
    #[serde(rename = "review.close")]
    Close { token: String },
    #[serde(rename = "review.open_file")]
    OpenFile { token: String, path: String },
}

/// A diff path from the page: relative, no `..`, not empty.
fn page_path(path: String) -> Result<String, String> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part == "..")
        || path.contains('\0');
    if bad {
        Err(format!("review page sent an invalid path {path:?}"))
    } else {
        Ok(path)
    }
}

/// Parses one message body from the Viewer page.
pub fn parse_ipc(body: &str) -> Result<ReviewIpc, String> {
    let wire: WireIpc = serde_json::from_str(body)
        .map_err(|error| format!("not a review message ({error}): {body}"))?;
    Ok(match wire {
        WireIpc::Source {
            token,
            source,
            hide_noise,
        } => ReviewIpc::SetSource {
            token,
            source: match source.as_str() {
                "uncommitted" => DiffSource::Uncommitted,
                "vs_base" => DiffSource::VsBase,
                other => return Err(format!("review page sent an unknown source {other:?}")),
            },
            hide_noise,
        },
        WireIpc::Viewed {
            token,
            path,
            viewed,
        } => ReviewIpc::SetViewed {
            token,
            path: page_path(path)?,
            viewed,
        },
        WireIpc::Close { token } => ReviewIpc::Close { token },
        WireIpc::OpenFile { token, path } => ReviewIpc::OpenFile {
            token,
            path: page_path(path)?,
        },
    })
}

// ---------------------------------------------------------------------------
// The page.

const PAGE_CSS: &str = include_str!("../assets/review_pane.css");
const PAGE_JS: &str = include_str!("../assets/review_pane.js");
/// Vendored, inlined (no network at runtime); license in `assets/vendor/`.
const HIGHLIGHT_JS: &str = include_str!("../assets/vendor/highlight.min.js");

/// Escapes text for HTML content and quoted attribute values. Every piece
/// of diff text goes through here: diff content is untrusted.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What the page body shows.
#[derive(Debug, Clone, Copy)]
pub enum PageState<'a> {
    Loading,
    Error(&'a str),
    /// The diff with each file's viewed state, in `diff.files` order.
    Ready {
        diff: &'a ReviewDiff,
        viewed: &'a [ViewedState],
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ReviewPage<'a> {
    pub target: &'a ReviewTarget,
    pub source: DiffSource,
    pub state: PageState<'a>,
    pub dark: bool,
    pub hide_noise: bool,
    /// Echoed in every IPC message.
    pub token: &'a str,
    /// The CSP nonce for the page's inline style and scripts.
    pub nonce: &'a str,
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {one}s")
    }
}

fn counts_html(additions: usize, deletions: usize) -> String {
    format!("<span class=\"ad\">+{additions}</span><span class=\"de\">\u{2212}{deletions}</span>")
}

/// The whole page: CSP, toolbar, file tree and file sections.
pub fn render_page(page: &ReviewPage) -> String {
    let nonce = escape_html(page.nonce);
    let mut html = String::with_capacity(64 * 1024);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"");
    html.push_str(if page.dark { "dark" } else { "light" });
    html.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str(&format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; img-src 'none'; \
         font-src 'none'; connect-src 'none'; media-src 'none'; object-src 'none'; \
         frame-src 'none'; base-uri 'none'; form-action 'none'\">\n"
    ));
    html.push_str("<title>Review</title>\n");
    html.push_str(&format!(
        "<style nonce=\"{nonce}\">\n{PAGE_CSS}\n</style>\n"
    ));
    html.push_str("</head>\n");
    html.push_str(&format!(
        "<body class=\"{}\" data-token=\"{}\">\n",
        if page.hide_noise { "hide-noise" } else { "" },
        escape_html(page.token)
    ));
    render_toolbar(&mut html, page);
    html.push_str("<div id=\"body\">\n");
    match page.state {
        PageState::Loading => {
            html.push_str(
                "<main id=\"files\"><p class=\"empty\">Loading changes\u{2026}</p></main>\n",
            );
        }
        PageState::Error(error) => {
            html.push_str(&format!(
                "<main id=\"files\"><p class=\"empty error\">{}</p></main>\n",
                escape_html(error)
            ));
        }
        PageState::Ready { diff, viewed } => {
            render_tree(&mut html, diff, viewed);
            render_files(&mut html, page, diff, viewed);
        }
    }
    html.push_str("</div>\n");
    html.push_str(&format!(
        "<script nonce=\"{nonce}\">\n{HIGHLIGHT_JS}\n</script>\n"
    ));
    html.push_str(&format!(
        "<script nonce=\"{nonce}\">\n{PAGE_JS}\n</script>\n"
    ));
    html.push_str("</body>\n</html>\n");
    html
}

fn render_toolbar(html: &mut String, page: &ReviewPage) {
    let target = page.target;
    html.push_str("<header id=\"toolbar\">\n<div class=\"row\">");
    html.push_str(&format!(
        "<span class=\"branch\">\u{2387} {}</span><span class=\"repo\">\u{00B7} {}</span>",
        escape_html(&target.branch),
        escape_html(&target.repo)
    ));
    let on = |source: DiffSource| if page.source == source { " on" } else { "" };
    let vs_label = match &target.base {
        Some(base) => format!("vs {}", base.reference),
        None => "vs base".to_string(),
    };
    html.push_str(&format!(
        "<div class=\"seg\" role=\"group\" aria-label=\"Diff source\">\
         <button type=\"button\" data-source=\"uncommitted\" class=\"src{}\">Uncommitted</button>\
         <button type=\"button\" data-source=\"vs_base\" class=\"src{}\"{}>{}</button></div>",
        on(DiffSource::Uncommitted),
        on(DiffSource::VsBase),
        if target.base.is_none() {
            " disabled"
        } else {
            ""
        },
        escape_html(&vs_label)
    ));
    html.push_str(&format!(
        "<label class=\"toggle\"><input type=\"checkbox\" id=\"hide-noise\"{}> Hide tests &amp; generated</label>",
        if page.hide_noise { " checked" } else { "" }
    ));
    html.push_str(
        "<span class=\"spacer\"></span><button type=\"button\" id=\"close\">Close</button></div>\n",
    );

    let mut facts = vec![
        escape_html(&target.checkout.display().to_string()),
        format!("owner: {}", escape_html(&target.owner)),
    ];
    match &target.base {
        Some(base) => {
            let mut copy = format!(
                "base {} ({})",
                escape_html(&base.reference),
                escape_html(&base.describe())
            );
            if let PageState::Ready { diff, .. } = page.state {
                if let (DiffSource::VsBase, Some(merge_base)) = (diff.source, &diff.merge_base) {
                    copy.push_str(&format!(" @ {} (merge-base)", escape_html(merge_base)));
                }
            }
            facts.push(copy);
        }
        None => facts.push(
            "no base: no task base, tracked branch, or develop/main/master/trunk here".to_string(),
        ),
    }
    html.push_str(&format!(
        "<div class=\"facts\">{}</div>\n",
        facts.join(" \u{00B7} ")
    ));

    if let PageState::Ready { diff, .. } = page.state {
        let all = diff.totals.all();
        html.push_str("<div class=\"totals\">");
        html.push_str(&format!(
            "<span class=\"tot all\">{} {}</span>",
            plural(all.files, "file"),
            counts_html(all.additions, all.deletions)
        ));
        for kind in FileKind::ALL {
            let totals = diff.totals.get(kind);
            if totals.files == 0 {
                continue;
            }
            html.push_str(&format!(
                "<span class=\"tot k-{label}\"><span class=\"kb k-{label}\">{label}</span> {} {}</span>",
                totals.files,
                counts_html(totals.additions, totals.deletions),
                label = kind.label()
            ));
        }
        if diff.unlisted > 0 {
            html.push_str(&format!(
                "<span class=\"tot more\">+{} not listed</span>",
                plural(diff.unlisted, "file")
            ));
        }
        html.push_str("</div>\n");
    }
    html.push_str("</header>\n");
}

/// A folder of the file tree.
#[derive(Default)]
struct TreeDir<'a> {
    name: &'a str,
    dirs: Vec<TreeDir<'a>>,
    files: Vec<usize>,
}

impl<'a> TreeDir<'a> {
    fn insert(&mut self, parts: &[&'a str], index: usize) {
        match parts {
            [] => {}
            [_file] => self.files.push(index),
            [dir, rest @ ..] => {
                let position = match self.dirs.iter().position(|d| d.name == *dir) {
                    Some(position) => position,
                    None => {
                        self.dirs.push(TreeDir {
                            name: dir,
                            ..TreeDir::default()
                        });
                        self.dirs.len() - 1
                    }
                };
                self.dirs[position].insert(rest, index);
            }
        }
    }

    fn all_noise(&self, files: &[ReviewFile]) -> bool {
        self.files.iter().all(|&index| files[index].kind.is_noise())
            && self.dirs.iter().all(|dir| dir.all_noise(files))
    }
}

fn render_tree(html: &mut String, diff: &ReviewDiff, viewed: &[ViewedState]) {
    let mut root = TreeDir::default();
    for (index, file) in diff.files.iter().enumerate() {
        let parts: Vec<&str> = file.path.split('/').collect();
        root.insert(&parts, index);
    }
    let noise = diff
        .files
        .iter()
        .filter(|file| file.kind.is_noise())
        .count();
    html.push_str("<nav id=\"tree\" aria-label=\"Changed files\">\n");
    if diff.files.is_empty() {
        html.push_str("<p class=\"empty\">No files</p>\n");
    }
    render_tree_dir(html, &root, diff, viewed);
    if noise > 0 {
        html.push_str(&format!(
            "<p class=\"noise-note\">{} hidden (tests &amp; generated)</p>\n",
            plural(noise, "file")
        ));
    }
    html.push_str("</nav>\n");
}

fn render_tree_dir(html: &mut String, dir: &TreeDir, diff: &ReviewDiff, viewed: &[ViewedState]) {
    for child in &dir.dirs {
        let noise = if child.all_noise(&diff.files) {
            " noise"
        } else {
            ""
        };
        html.push_str(&format!(
            "<details class=\"dir{noise}\" open><summary>{}</summary><div class=\"kids\">\n",
            escape_html(child.name)
        ));
        render_tree_dir(html, child, diff, viewed);
        html.push_str("</div></details>\n");
    }
    for &index in &dir.files {
        let file = &diff.files[index];
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let mut classes = format!("tf k-{}", file.kind.label());
        if file.kind.is_noise() {
            classes.push_str(" noise");
        }
        match viewed.get(index) {
            Some(ViewedState::Viewed) => classes.push_str(" viewed"),
            Some(ViewedState::ChangedSinceViewed) => classes.push_str(" stale"),
            _ => {}
        }
        html.push_str(&format!(
            "<button type=\"button\" class=\"{classes}\" data-idx=\"{index}\" title=\"{}\">\
             <span class=\"st s-{}\">{}</span><span class=\"nm\">{}</span>\
             <span class=\"kb k-{}\">{}</span>{}</button>\n",
            escape_html(&file.path),
            file.status.label(),
            file.status.letter(),
            escape_html(name),
            file.kind.label(),
            file.kind.label(),
            counts_html(file.additions, file.deletions)
        ));
    }
}

fn render_files(html: &mut String, page: &ReviewPage, diff: &ReviewDiff, viewed: &[ViewedState]) {
    html.push_str("<main id=\"files\">\n");
    if diff.files.is_empty() {
        let empty = match (page.source, &page.target.base) {
            (DiffSource::Uncommitted, _) => "No uncommitted changes".to_string(),
            (DiffSource::VsBase, Some(base)) => format!("No changes against {}", base.reference),
            (DiffSource::VsBase, None) => "No base branch".to_string(),
        };
        html.push_str(&format!("<p class=\"empty\">{}</p>\n", escape_html(&empty)));
    }
    for (index, file) in diff.files.iter().enumerate() {
        render_file(
            html,
            index,
            file,
            viewed.get(index).copied().unwrap_or(ViewedState::Unviewed),
        );
    }
    html.push_str("</main>\n");
}

fn render_file(html: &mut String, index: usize, file: &ReviewFile, viewed: ViewedState) {
    let mut classes = format!("file k-{} s-{}", file.kind.label(), file.status.label());
    if file.kind.is_noise() {
        classes.push_str(" noise");
    }
    match viewed {
        ViewedState::Viewed => classes.push_str(" viewed collapsed"),
        ViewedState::ChangedSinceViewed => classes.push_str(" stale"),
        ViewedState::Unviewed => {}
    }
    if file.omitted {
        classes.push_str(" collapsed");
    }
    html.push_str(&format!(
        "<section class=\"{classes}\" id=\"f{index}\" data-idx=\"{index}\" data-path=\"{}\">\n",
        escape_html(&file.path)
    ));
    // Header.
    let path = match &file.old_path {
        Some(old) => format!(
            "{} <span class=\"arrow\">\u{2192}</span> {}",
            escape_html(old),
            escape_html(&file.path)
        ),
        None => escape_html(&file.path),
    };
    html.push_str(&format!(
        "<div class=\"fh\"><button type=\"button\" class=\"chev\" aria-label=\"Collapse or expand\"></button>\
         <span class=\"st s-{}\" title=\"{}\">{}</span><span class=\"path\">{path}</span>\
         <span class=\"kb k-{}\">{}</span>{}",
        file.status.label(),
        file.status.label(),
        file.status.letter(),
        file.kind.label(),
        file.kind.label(),
        counts_html(file.additions, file.deletions)
    ));
    if file.omitted {
        html.push_str("<span class=\"badge big\">too large to show here</span>");
    }
    html.push_str("<span class=\"badge changed\">Changed since viewed</span>");
    html.push_str("<span class=\"spacer\"></span>");
    if file.status != FileStatus::Deleted {
        html.push_str("<button type=\"button\" class=\"open\">Open</button>");
    }
    let can_mark = file.content_id.is_some();
    html.push_str(&format!(
        "<label class=\"vw\"{}><input type=\"checkbox\" class=\"viewed\"{}{}> Viewed</label></div>\n",
        if can_mark {
            ""
        } else {
            " title=\"This version could not be hashed, so it cannot be marked viewed\""
        },
        if viewed == ViewedState::Viewed {
            " checked"
        } else {
            ""
        },
        if can_mark { "" } else { " disabled" }
    ));
    // Body.
    html.push_str("<div class=\"fb\">");
    if file.omitted {
        html.push_str("<p class=\"note\">Too large to show here.</p>");
    } else if file.status == FileStatus::Binary {
        html.push_str("<p class=\"note\">Binary file; not shown.</p>");
    } else if let Some(note) = &file.note {
        html.push_str(&format!("<p class=\"note\">{}</p>", escape_html(note)));
    } else if file.hunks.is_empty() {
        let why = if file.additions + file.deletions == 0 {
            if file.status == FileStatus::Untracked {
                "Empty file."
            } else {
                "No text changes (mode, rename or metadata only)."
            }
        } else {
            "Not shown: the diff reached its size cap."
        };
        html.push_str(&format!("<p class=\"note\">{why}</p>"));
    } else {
        html.push_str(
            "<table class=\"d\"><colgroup><col class=\"cn\"><col class=\"cn\"><col></colgroup><tbody>\n",
        );
        for hunk in &file.hunks {
            html.push_str(&format!(
                "<tr class=\"hh\"><td colspan=\"3\">{}</td></tr>\n",
                escape_html(&hunk.header)
            ));
            for line in &hunk.lines {
                let class = match line.op {
                    LineOp::Context => "x",
                    LineOp::Add => "a",
                    LineOp::Del => "r",
                };
                let number = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                html.push_str(&format!(
                    "<tr class=\"{class}\"><td class=\"o\" data-n=\"{}\"></td><td class=\"n\" data-n=\"{}\"></td><td class=\"c\">{}</td></tr>\n",
                    number(line.old_no),
                    number(line.new_no),
                    escape_html(&line.text)
                ));
            }
        }
        html.push_str("</tbody></table>");
        if file.truncated {
            html.push_str(&format!(
                "<p class=\"note\">Cut at {} lines shown (caps: {} lines or {} KB per file).</p>",
                file.shown_lines(),
                FILE_LINE_CAP,
                FILE_BYTE_CAP / 1024
            ));
        }
    }
    html.push_str("</div>\n</section>\n");
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_ports_t3_cases() {
        // T3 Code's changedFileKind.test.ts, with "mocks" and "tests" both
        // folding into Test.
        for (path, kind) in [
            ("src/contest.py", FileKind::Code),
            ("test/fixtures/pr.json", FileKind::Test),
            ("pnpm-lock.yaml", FileKind::Generated),
            ("README.md", FileKind::Docs),
            ("tests/README.md", FileKind::Test),
        ] {
            assert_eq!(classify_path(path), kind, "{path}");
        }
    }

    #[test]
    fn classifier_follows_t3_naming_rules() {
        for (path, kind) in [
            // Generated: lockfiles, build output, generated dirs.
            ("Cargo.lock", FileKind::Generated),
            ("web/yarn.lock", FileKind::Generated),
            ("src/api.generated.ts", FileKind::Generated),
            ("proto/acme.pb.go", FileKind::Generated),
            ("acme_pb2_grpc.py", FileKind::Generated),
            ("dist/app.min.js", FileKind::Generated),
            ("dist/app.js.map", FileKind::Generated),
            ("src/__generated__/schema.ts", FileKind::Generated),
            ("lib/Generated/x.swift", FileKind::Generated),
            // Mocks and fixtures.
            ("src/__snapshots__/view.ts.snap", FileKind::Test),
            ("src/user.mock.ts", FileKind::Test),
            ("src/mockData.ts", FileKind::Test),
            ("src/mock_api.rs", FileKind::Test),
            ("src/mockery.go", FileKind::Code),
            ("pkg/testdata/in.txt", FileKind::Test),
            // Tests.
            ("src/app.test.tsx", FileKind::Test),
            ("src/App.Spec.ts", FileKind::Test),
            ("pkg/server_test.go", FileKind::Test),
            ("test_acme.py", FileKind::Test),
            ("conftest.py", FileKind::Test),
            ("vitest.setup.ts", FileKind::Test),
            ("src/FooTest.java", FileKind::Test),
            ("src/FooTests.cs", FileKind::Test),
            ("src/Latest.java", FileKind::Code),
            ("src/footest.java", FileKind::Code),
            ("e2e/login.ts", FileKind::Test),
            // Docs, but not tool input.
            ("docs/guide.MD", FileKind::Docs),
            ("notes.txt", FileKind::Docs),
            ("requirements-dev.txt", FileKind::Config),
            ("CMakeLists.txt", FileKind::Config),
            // Config.
            ("Cargo.toml", FileKind::Config),
            ("tsconfig.json", FileKind::Config),
            (".github/workflows/ci.yml", FileKind::Config),
            (".gitignore", FileKind::Config),
            (".env.local", FileKind::Config),
            // Code, with Windows separators too.
            ("src/main.rs", FileKind::Code),
            ("src\\tests\\a.rs", FileKind::Test),
        ] {
            assert_eq!(classify_path(path), kind, "{path}");
        }
    }

    fn summary_bytes(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        }
        out
    }

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const Z: &str = "0000000000000000000000000000000000000000";

    #[test]
    fn raw_numstat_parses_modified_renamed_deleted_and_binary() {
        let raw_m = format!(":100644 100644 {A} {B} M");
        let raw_r = format!(":100644 100644 {A} {B} R087");
        let raw_d = format!(":100644 000000 {A} {Z} D");
        let raw_b = format!(":100644 100644 {A} {B} M");
        let bytes = summary_bytes(&[
            &raw_m,
            "src/a.rs",
            &raw_r,
            "src/old name.rs",
            "src/new name.rs",
            &raw_d,
            "gone.txt",
            &raw_b,
            "logo.png",
            "3\t1\tsrc/a.rs",
            "2\t2\t",
            "src/old name.rs",
            "src/new name.rs",
            "0\t4\tgone.txt",
            "-\t-\tlogo.png",
        ]);
        let entries = parse_raw_numstat(&bytes).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            (
                entries[0].path.as_str(),
                entries[0].additions,
                entries[0].deletions
            ),
            ("src/a.rs", 3, 1)
        );
        assert_eq!(entries[1].status, 'R');
        assert_eq!(entries[1].old_path.as_deref(), Some("src/old name.rs"));
        assert_eq!(entries[1].path, "src/new name.rs");
        assert_eq!((entries[1].additions, entries[1].deletions), (2, 2));
        assert_eq!(tracked_status(&entries[1]), FileStatus::Renamed);
        assert_eq!(tracked_status(&entries[2]), FileStatus::Deleted);
        assert_eq!(entries[2].old_blob, A);
        assert!(entries[3].binary);
        assert_eq!(tracked_status(&entries[3]), FileStatus::Binary);
        assert_eq!(entries[0].new_blob, B);

        // A numstat record for a file with no raw record is an error.
        let bad = summary_bytes(&[&raw_m, "a", "1\t1\tb"]);
        assert!(parse_raw_numstat(&bad).is_err());
        assert_eq!(parse_raw_numstat(b"").unwrap(), Vec::new());
    }

    #[test]
    fn diff_headers_quote_like_git() {
        assert_eq!(quote_git_path("a/", "src/x y.rs"), "a/src/x y.rs");
        assert_eq!(quote_git_path("b/", "caf\u{e9}.md"), "b/caf\u{e9}.md");
        assert_eq!(quote_git_path("a/", "tab\there"), "\"a/tab\\there\"");
        assert_eq!(quote_git_path("a/", "q\"x\\y"), "\"a/q\\\"x\\\\y\"");
        assert_eq!(quote_git_path("a/", "bell\u{1}"), "\"a/bell\\001\"");
    }

    fn feed(lines: &[&str]) -> PatchParser {
        let mut parser = PatchParser::default();
        for line in lines {
            parser.feed(line);
        }
        parser
    }

    #[test]
    fn patch_parses_hunks_numbers_and_tricky_content() {
        let parser = feed(&[
            "diff --git a/src/a.rs b/src/a.rs",
            "index 1..2 100644",
            "--- a/src/a.rs",
            "+++ b/src/a.rs",
            "@@ -10,3 +10,4 @@ fn x() {",
            " keep",
            "-old",
            "+new",
            "+++ looks like a header",
            "\\ No newline at end of file",
            "diff --git a/b.bin b/b.bin",
            "Binary files a/b.bin and b/b.bin differ",
            "diff --git a/gone.txt b/gone.txt",
            "deleted file mode 100644",
            "--- a/gone.txt",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-bye",
        ]);
        assert_eq!(parser.files.len(), 3);
        let a = &parser.files[0];
        assert_eq!(a.header, "diff --git a/src/a.rs b/src/a.rs");
        assert_eq!(a.hunks.len(), 1);
        let lines: Vec<(LineOp, Option<u32>, Option<u32>, &str)> = a.hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.old_no, l.new_no, l.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (LineOp::Context, Some(10), Some(10), "keep"),
                (LineOp::Del, Some(11), None, "old"),
                (LineOp::Add, None, Some(11), "new"),
                (LineOp::Add, None, Some(12), "++ looks like a header"),
            ]
        );
        assert!(parser.files[1].binary);
        assert!(parser.files[1].hunks.is_empty());
        let gone = &parser.files[2].hunks[0].lines;
        assert_eq!(gone.len(), 1);
        assert_eq!((gone[0].op, gone[0].old_no), (LineOp::Del, Some(1)));
        assert!(!parser.stopped);
    }

    #[test]
    fn patch_cuts_a_file_at_its_line_cap() {
        let mut lines = vec![
            "diff --git a/big.rs b/big.rs".to_string(),
            format!("@@ -0,0 +1,{} @@", FILE_LINE_CAP + 5),
        ];
        for i in 0..FILE_LINE_CAP + 5 {
            lines.push(format!("+line {i}"));
        }
        lines.push("diff --git a/next.rs b/next.rs".to_string());
        lines.push("@@ -1 +1 @@".to_string());
        lines.push("-a".to_string());
        lines.push("+b".to_string());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parser = feed(&refs);
        assert!(parser.files[0].truncated);
        assert_eq!(parser.files[0].hunks[0].lines.len(), FILE_LINE_CAP);
        assert!(!parser.files[1].truncated);
        assert_eq!(parser.files[1].hunks[0].lines.len(), 2);
    }

    #[test]
    fn patch_stops_at_the_total_byte_cap() {
        let mut parser = PatchParser::default();
        // Three half-cap lines per file: the third is past the file's byte
        // cap, so each file shows two and is cut.
        let long = format!("+{}", "x".repeat(FILE_BYTE_CAP / 2));
        for file in 0..64 {
            if !parser.feed(&format!("diff --git a/f{file} b/f{file}")) {
                break;
            }
            parser.feed("@@ -0,0 +1,3 @@");
            for _ in 0..3 {
                if !parser.feed(&long) {
                    break;
                }
            }
        }
        assert!(parser.stopped);
        assert_eq!(parser.budget.bytes, TOTAL_BYTE_CAP);
        let files = TOTAL_BYTE_CAP / FILE_BYTE_CAP;
        assert_eq!(parser.files.len(), files);
        assert!(parser.files.iter().all(|file| file.truncated));
        assert_eq!(parser.files[0].hunks[0].lines.len(), 2);
        // Nothing more is read once stopped.
        assert!(!parser.feed("diff --git a/late b/late"));
        assert_eq!(parser.files.len(), files);
    }

    #[test]
    fn untracked_text_becomes_added_lines_and_is_cut() {
        let mut budget = Budget::default();
        let (hunks, shown, truncated) = untracked_hunks(b"one\r\ntwo\nthree\n", false, &mut budget);
        assert_eq!((shown, truncated), (3, false));
        assert_eq!(hunks[0].header, "@@ -0,0 +1,3 @@");
        let texts: Vec<&str> = hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "three"]);
        assert_eq!(hunks[0].lines[2].new_no, Some(3));
        assert!(hunks[0].lines.iter().all(|l| l.op == LineOp::Add));

        let many: String = (0..FILE_LINE_CAP + 10).map(|i| format!("{i}\n")).collect();
        let (hunks, shown, truncated) = untracked_hunks(many.as_bytes(), false, &mut budget);
        assert_eq!((shown, truncated), (FILE_LINE_CAP, true));
        assert_eq!(hunks[0].lines.len(), FILE_LINE_CAP);

        // Read only up to the byte cap: cut, even when every line fits.
        let (_, shown, truncated) = untracked_hunks(b"a\nb", true, &mut budget);
        assert_eq!((shown, truncated), (2, true));

        let (hunks, shown, _) = untracked_hunks(b"", false, &mut budget);
        assert!(hunks.is_empty());
        assert_eq!(shown, 0);
    }

    #[test]
    fn untracked_binary_is_sniffed() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        let binary = dir.path().join("logo.png");
        std::fs::write(&text, "hello\n").unwrap();
        std::fs::write(&binary, [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        assert_eq!(sniff_untracked(&text), Sniff::Text { size: 6 });
        assert_eq!(sniff_untracked(&binary), Sniff::Binary);
        assert!(matches!(sniff_untracked(dir.path()), Sniff::Unreadable(_)));
        assert!(matches!(
            sniff_untracked(&dir.path().join("missing")),
            Sniff::Unreadable(_)
        ));
    }

    #[test]
    fn tree_order_puts_folders_first() {
        let mut paths = vec![
            "z.rs",
            "src/b.rs",
            "Cargo.lock",
            "src/a/x.rs",
            "a.md",
            "src/a.rs",
        ];
        paths.sort_by(|a, b| tree_order(a, b));
        assert_eq!(
            paths,
            [
                "src/a/x.rs",
                "src/a.rs",
                "src/b.rs",
                "Cargo.lock",
                "a.md",
                "z.rs"
            ]
        );
    }

    #[test]
    fn admission_prefers_code_and_skips_what_does_not_fit() {
        let costs = [
            (FileKind::Generated, 15_000),
            (FileKind::Code, 1_500),
            (FileKind::Test, 1_900),
            (FileKind::Code, 50_000),
            (FileKind::Docs, 10),
        ];
        // Kind order: code 1,500 + 2,000 (capped), docs 10, test 1,900 =
        // 5,410; the generated file's 2,000 does not fit in what is left.
        assert_eq!(admit(&costs, 7_000), HashSet::from([1, 3, 4, 2]));
        assert_eq!(admit(&costs, 3_510), HashSet::from([1, 3, 4]));
        assert_eq!(admit(&costs, 20_000).len(), 5);
        // A file that does not fit is skipped; a smaller one after it fits.
        let costs = [
            (FileKind::Code, 1_500),
            (FileKind::Code, 1_000),
            (FileKind::Code, 400),
        ];
        assert_eq!(admit(&costs, 1_900), HashSet::from([0, 2]));
    }

    fn file(path: &str, kind: FileKind, adds: usize, dels: usize) -> ReviewFile {
        ReviewFile {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            kind,
            additions: adds,
            deletions: dels,
            truncated: false,
            omitted: false,
            note: None,
            hunks: Vec::new(),
            content_id: Some(format!("id-{path}")),
        }
    }

    #[test]
    fn totals_split_by_kind() {
        let files = [
            file("src/a.rs", FileKind::Code, 3, 1),
            file("src/b.rs", FileKind::Code, 2, 0),
            file("tests/a.rs", FileKind::Test, 10, 2),
            file("Cargo.lock", FileKind::Generated, 40, 40),
        ];
        let totals = ReviewTotals::of(&files);
        assert_eq!(
            totals.get(FileKind::Code),
            KindTotals {
                files: 2,
                additions: 5,
                deletions: 1
            }
        );
        assert_eq!(totals.get(FileKind::Test).additions, 10);
        assert_eq!(totals.get(FileKind::Docs), KindTotals::default());
        assert_eq!(
            totals.all(),
            KindTotals {
                files: 4,
                additions: 55,
                deletions: 43
            }
        );
    }

    #[test]
    fn viewed_marks_go_stale_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = Path::new("/tmp/acme/portal");
        let mut a = file("src/a.rs", FileKind::Code, 1, 0);
        let b = file("src/b.rs", FileKind::Code, 1, 0);
        let mut marks = ViewedMarks::load(dir.path()).unwrap();
        assert_eq!(marks.state(checkout, &a), ViewedState::Unviewed);
        assert!(marks.set(checkout, &a, true));
        assert!(marks.set(checkout, &b, true));
        assert_eq!(marks.state(checkout, &a), ViewedState::Viewed);
        // Another checkout of the same repo has its own marks.
        assert_eq!(
            marks.state(Path::new("/tmp/acme/portal-prj-101"), &a),
            ViewedState::Unviewed
        );
        marks.save(dir.path()).unwrap();
        let mut marks = ViewedMarks::load(dir.path()).unwrap();
        // The content changed since it was viewed.
        a.content_id = Some("id-new".to_string());
        assert_eq!(marks.state(checkout, &a), ViewedState::ChangedSinceViewed);
        // A file with no content id cannot be marked.
        let mut unhashed = file("src/c.rs", FileKind::Code, 1, 0);
        unhashed.content_id = None;
        assert!(!marks.set(checkout, &unhashed, true));
        // b left every diff: pruned; a is still live.
        let live = BTreeSet::from(["src/a.rs".to_string()]);
        assert!(marks.prune(checkout, &live));
        assert!(!marks.prune(checkout, &live));
        assert_eq!(marks.state(checkout, &b), ViewedState::Unviewed);
        assert_eq!(marks.state(checkout, &a), ViewedState::ChangedSinceViewed);
        // Unmarking the last file drops the checkout.
        assert!(marks.set(checkout, &a, false));
        assert!(marks.checkouts.is_empty());
        // A corrupt file is an error, never silently replaced.
        std::fs::write(dir.path().join(VIEWED_FILE), "not json").unwrap();
        assert!(ViewedMarks::load(dir.path()).is_err());
    }

    #[test]
    fn ipc_messages_parse() {
        assert_eq!(
            parse_ipc(
                r#"{"type":"review.source","token":"t1","source":"vs_base","hide_noise":false}"#
            ),
            Ok(ReviewIpc::SetSource {
                token: "t1".into(),
                source: DiffSource::VsBase,
                hide_noise: false
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"review.viewed","token":"t1","path":"src/a.rs","viewed":true}"#),
            Ok(ReviewIpc::SetViewed {
                token: "t1".into(),
                path: "src/a.rs".into(),
                viewed: true
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"review.close","token":"t1"}"#),
            Ok(ReviewIpc::Close { token: "t1".into() })
        );
        let open =
            parse_ipc(r#"{"type":"review.open_file","token":"t1","path":"docs/a b.md"}"#).unwrap();
        assert_eq!(open.token(), "t1");
        assert_eq!(
            open,
            ReviewIpc::OpenFile {
                token: "t1".into(),
                path: "docs/a b.md".into()
            }
        );
        for bad in [
            r#"{"type":"review.source","token":"t1","source":"commits","hide_noise":true}"#,
            r#"{"type":"review.open_file","token":"t1","path":"../etc/passwd"}"#,
            r#"{"type":"review.open_file","token":"t1","path":"/etc/passwd"}"#,
            r#"{"type":"review.viewed","token":"t1","path":""}"#,
            r#"{"type":"review.close"}"#,
            r#"{"type":"submit","tabId":1,"text":"x"}"#,
            "not json",
        ] {
            assert!(parse_ipc(bad).is_err(), "{bad}");
        }
    }

    fn hostile_diff() -> (ReviewTarget, ReviewDiff) {
        let hostile = "</script><img src=x onerror=alert(1)>";
        let target = ReviewTarget {
            checkout: PathBuf::from("/tmp/acme/portal"),
            repo: format!("portal{hostile}"),
            branch: format!("work/prj-101{hostile}"),
            owner: "coordinator".into(),
            base: None,
        };
        let mut f = file(&format!("src/{hostile}.rs"), FileKind::Code, 1, 1);
        f.hunks = vec![ReviewHunk {
            header: format!("@@ -1 +1 @@ {hostile}"),
            lines: vec![
                ReviewLine {
                    op: LineOp::Del,
                    old_no: Some(1),
                    new_no: None,
                    text: hostile.to_string(),
                },
                ReviewLine {
                    op: LineOp::Add,
                    old_no: None,
                    new_no: Some(1),
                    text: format!("let s = \"{hostile}\"; // ' & \""),
                },
            ],
        }];
        let files = vec![f];
        let diff = ReviewDiff {
            checkout: target.checkout.clone(),
            branch: target.branch.clone(),
            base: None,
            source: DiffSource::Uncommitted,
            merge_base: None,
            totals: ReviewTotals::of(&files),
            files,
            unlisted: 0,
        };
        (target, diff)
    }

    #[test]
    fn page_escapes_hostile_text() {
        assert_eq!(
            escape_html("</script><img src=x onerror=\"a('b')\">&"),
            "&lt;/script&gt;&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;"
        );
        let (target, diff) = hostile_diff();
        let viewed = [ViewedState::Unviewed];
        let html = render_page(&ReviewPage {
            target: &target,
            source: DiffSource::Uncommitted,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: true,
            hide_noise: true,
            token: "tok\"en",
            nonce: "n0nce",
        });
        assert!(!html.contains("<img"), "an unescaped tag reached the page");
        assert!(!html.contains("onerror=alert(1)>"));
        assert!(html.contains("&lt;/script&gt;&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("data-token=\"tok&quot;en\""));
        // Only the page's own three elements carry the nonce, and the CSP
        // allows nothing else.
        assert_eq!(html.matches("nonce=\"n0nce\"").count(), 3);
        assert!(html.contains("default-src 'none'; script-src 'nonce-n0nce'"));
        // Every literal </script> left closes one of the page's scripts.
        assert_eq!(html.matches("</script>").count(), 2);
        assert!(
            !html.contains("src=\"http"),
            "no element loads a remote URL"
        );
    }

    #[test]
    fn page_marks_viewed_and_stale_files() {
        let (target, mut diff) = hostile_diff();
        diff.files = vec![
            file("src/a.rs", FileKind::Code, 1, 0),
            file("src/b.rs", FileKind::Code, 1, 0),
            file("Cargo.lock", FileKind::Generated, 1, 0),
        ];
        diff.files[2].omitted = true;
        diff.totals = ReviewTotals::of(&diff.files);
        let viewed = [
            ViewedState::Viewed,
            ViewedState::ChangedSinceViewed,
            ViewedState::Unviewed,
        ];
        let html = render_page(&ReviewPage {
            target: &target,
            source: DiffSource::Uncommitted,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: false,
            hide_noise: false,
            token: "t",
            nonce: "n",
        });
        assert!(html.contains("class=\"file k-code s-modified viewed collapsed\" id=\"f0\""));
        assert!(html.contains("class=\"file k-code s-modified stale\" id=\"f1\""));
        assert!(html.contains("class=\"file k-generated s-modified noise collapsed\" id=\"f2\""));
        assert!(html.contains("too large to show here"));
        assert!(html.contains("data-theme=\"light\""));
        assert!(html
            .contains("<button type=\"button\" data-source=\"vs_base\" class=\"src\" disabled>"));
    }

    #[test]
    fn loading_and_error_pages_render() {
        let (target, _) = hostile_diff();
        for state in [PageState::Loading, PageState::Error("git <failed>")] {
            let html = render_page(&ReviewPage {
                target: &target,
                source: DiffSource::VsBase,
                state,
                dark: true,
                hide_noise: true,
                token: "t",
                nonce: "n",
            });
            assert!(html.contains("id=\"close\""));
            assert!(!html.contains("git <failed>"));
        }
    }

    // -----------------------------------------------------------------------
    // Real git.

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn target(checkout: &Path, base: Option<&str>) -> ReviewTarget {
        ReviewTarget {
            checkout: checkout.to_path_buf(),
            repo: "portal".into(),
            branch: "work/prj-101".into(),
            owner: "coordinator".into(),
            base: base.map(|reference| BaseChoice {
                reference: reference.to_string(),
                source: crate::workspace_overview::BaseSource::Local,
                considered: 1,
            }),
        }
    }

    #[tokio::test]
    async fn real_git_review_reads_every_kind_of_change() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("portal");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(repo.join("src/moved.rs"), &body).unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(repo.join("logo.png"), [0u8, 1, 2, 3]).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(&repo, &["checkout", "-q", "-b", "work/prj-101"]);
        std::fs::write(repo.join("src/c.rs"), "fn c() {}\n").unwrap();
        git(&repo, &["add", "src/c.rs"]);
        git(&repo, &["commit", "-q", "-m", "work"]);
        // Uncommitted: modified, renamed with an edit, deleted, binary,
        // untracked text (one past the line cap) and untracked binary.
        std::fs::write(repo.join("src/a.rs"), "fn a() {}\nfn b2() {}\n").unwrap();
        git(&repo, &["mv", "src/moved.rs", "src/renamed.rs"]);
        std::fs::write(repo.join("src/renamed.rs"), format!("{body}tail\n")).unwrap();
        git(&repo, &["add", "src/renamed.rs"]);
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("logo.png"), [0u8, 9, 9, 9]).unwrap();
        let long: String = (0..FILE_LINE_CAP + 1).map(|i| format!("n {i}\n")).collect();
        std::fs::write(repo.join("notes.md"), &long).unwrap();
        std::fs::write(repo.join("blob.dat"), [7u8, 0, 7]).unwrap();
        std::fs::write(repo.join("Cargo.lock"), "# generated\n").unwrap();

        let load = load_review(target(&repo, Some("main")), DiffSource::Uncommitted)
            .await
            .unwrap();
        let diff = &load.diff;
        let by_path = |path: &str| diff.file(path).unwrap_or_else(|| panic!("{path} missing"));
        let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "src/a.rs",
                "src/renamed.rs",
                "Cargo.lock",
                "blob.dat",
                "gone.txt",
                "logo.png",
                "notes.md"
            ]
        );
        let a = by_path("src/a.rs");
        assert_eq!(
            (a.status, a.kind, a.additions, a.deletions),
            (FileStatus::Modified, FileKind::Code, 1, 1)
        );
        let ops: Vec<(LineOp, &str)> = a.hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.text.as_str()))
            .collect();
        assert!(
            ops.contains(&(LineOp::Del, "fn b() {}")) && ops.contains(&(LineOp::Add, "fn b2() {}"))
        );
        // The working-tree hash, as git computes it.
        assert_eq!(
            a.content_id.as_deref(),
            Some(git_out(&repo, &["hash-object", "src/a.rs"]).as_str())
        );
        let renamed = by_path("src/renamed.rs");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("src/moved.rs"));
        assert_eq!(renamed.additions, 1);
        assert!(renamed.hunks[0]
            .lines
            .iter()
            .any(|l| l.op == LineOp::Add && l.text == "tail"));
        let gone = by_path("gone.txt");
        assert_eq!(gone.status, FileStatus::Deleted);
        assert_eq!(gone.hunks[0].lines[0].text, "bye");
        assert!(gone.content_id.as_deref().unwrap().starts_with("deleted:"));
        let logo = by_path("logo.png");
        assert_eq!(logo.status, FileStatus::Binary);
        assert!(logo.hunks.is_empty());
        let notes = by_path("notes.md");
        assert_eq!(
            (notes.status, notes.kind),
            (FileStatus::Untracked, FileKind::Docs)
        );
        assert!(notes.truncated);
        assert_eq!(notes.shown_lines(), FILE_LINE_CAP);
        let blob = by_path("blob.dat");
        assert_eq!(blob.status, FileStatus::Binary);
        assert!(blob.hunks.is_empty() && blob.content_id.is_some());
        let lock = by_path("Cargo.lock");
        assert_eq!((lock.kind, lock.additions), (FileKind::Generated, 1));
        // Binary assets classify as code, as in T3.
        assert_eq!(diff.totals.get(FileKind::Code).files, 4);
        assert_eq!(diff.merge_base, None);
        // Both sources' paths are live: src/c.rs is only in vs base.
        let live = load.live_paths.as_ref().unwrap();
        assert!(live.contains("src/c.rs") && live.contains("notes.md"));

        let vs_base = load_review(target(&repo, Some("main")), DiffSource::VsBase)
            .await
            .unwrap();
        let paths: Vec<&str> = vs_base.diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/c.rs"]);
        let c = &vs_base.diff.files[0];
        assert_eq!(c.status, FileStatus::Added);
        // A committed version's id is its blob.
        assert_eq!(
            c.content_id.as_deref(),
            Some(git_out(&repo, &["rev-parse", "HEAD:src/c.rs"]).as_str())
        );
        assert_eq!(vs_base.diff.merge_base.as_ref().map(String::len), Some(7));
        assert!(vs_base.live_paths.unwrap().contains("notes.md"));
        assert!(load_review(target(&repo, None), DiffSource::VsBase)
            .await
            .is_err());
    }
}
