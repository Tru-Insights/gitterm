use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use super::budget::*;
use super::model::*;

/// Bytes sniffed for a NUL to call a file binary (git's heuristic).
const BINARY_SNIFF_BYTES: usize = 8_000;

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
    pub fn is_submodule(&self) -> bool {
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
pub fn expected_header(entry: &SummaryEntry) -> String {
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

pub fn tracked_status(entry: &SummaryEntry) -> FileStatus {
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
