//! Maps Codex review output onto the delegation findings contract
//! (TRU-142 slice S2; contract pinned in `.plans/agent-handoff-ux.md` §8).
//!
//! Two sources, in order of preference:
//! 1. The session rollout (`rollout-*-<thread_id>.jsonl`): the last
//!    `event_msg` whose `payload.item.type` is `ExitedReviewMode` carries a
//!    structured `review_output`. See [`review_findings_from_rollout`].
//! 2. The rendered final message (the `-o` file or the last `agent_message`):
//!    see [`review_findings_from_text`], which falls back to the whole text as
//!    the summary with `structured: false` when the findings cannot be read.
//!
//! Both functions are pure apart from canonicalising paths to make them
//! repo-relative.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::tasks::{ReviewFinding, ReviewFindings, ReviewSeverity, ReviewVerdict, Reviewer};

const EXITED_REVIEW_MODE: &str = "ExitedReviewMode";
const FINDINGS_HEADINGS: [&str; 2] = ["\n\nFull review comments:\n\n", "\n\nReview comment:\n\n"];
const BODY_INDENT: &str = "  ";
const TITLE_LOCATION_SEPARATOR: &str = " \u{2014} ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReviewParseError {
    detail: String,
}

impl CodexReviewParseError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CodexReviewParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to read Codex review output: {}",
            self.detail
        )
    }
}

impl std::error::Error for CodexReviewParseError {}

#[derive(Debug, Deserialize)]
struct RolloutLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexReviewOutput {
    findings: Vec<CodexFinding>,
    overall_correctness: String,
    overall_explanation: String,
}

#[derive(Debug, Deserialize)]
struct CodexFinding {
    title: String,
    body: String,
    #[serde(default)]
    confidence_score: Option<f32>,
    priority: u8,
    code_location: CodexCodeLocation,
}

#[derive(Debug, Deserialize)]
struct CodexCodeLocation {
    absolute_file_path: PathBuf,
    line_range: CodexLineRange,
}

#[derive(Debug, Deserialize)]
struct CodexLineRange {
    start: u32,
    end: u32,
}

/// Reads the structured review from a Codex session rollout. Uses the last
/// `ExitedReviewMode` item; errors when the rollout has none, a line is not
/// JSON, or the review output does not match the pinned contract.
pub fn review_findings_from_rollout(
    rollout_jsonl: &str,
    repo_root: &Path,
    reviewer: Reviewer,
) -> Result<ReviewFindings, CodexReviewParseError> {
    let mut review_output = None;
    for (index, line) in rollout_jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: RolloutLine = serde_json::from_str(line).map_err(|error| {
            CodexReviewParseError::new(format!("rollout line {} is not JSON: {error}", index + 1))
        })?;
        if parsed.kind != "event_msg" {
            continue;
        }
        let Some(payload) = parsed.payload else {
            continue;
        };
        if payload.get("type").and_then(|value| value.as_str()) != Some("item_completed") {
            continue;
        }
        let Some(item) = payload.get("item") else {
            continue;
        };
        if item.get("type").and_then(|value| value.as_str()) != Some(EXITED_REVIEW_MODE) {
            continue;
        }
        let output = item.get("review_output").ok_or_else(|| {
            CodexReviewParseError::new(format!(
                "rollout line {} has {EXITED_REVIEW_MODE} without review_output",
                index + 1
            ))
        })?;
        review_output = Some((index + 1, output.clone()));
    }
    let (line_number, output) = review_output.ok_or_else(|| {
        CodexReviewParseError::new(format!("rollout has no {EXITED_REVIEW_MODE} item"))
    })?;
    let output: CodexReviewOutput = serde_json::from_value(output).map_err(|error| {
        CodexReviewParseError::new(format!(
            "review_output on rollout line {line_number} does not match the contract: {error}"
        ))
    })?;

    let mut findings = Vec::with_capacity(output.findings.len());
    for (index, finding) in output.findings.into_iter().enumerate() {
        let severity = ReviewSeverity::from_priority(finding.priority).ok_or_else(|| {
            CodexReviewParseError::new(format!(
                "finding {} has priority {}, expected 0-3",
                index + 1,
                finding.priority
            ))
        })?;
        findings.push(ReviewFinding {
            id: finding_id(index),
            severity,
            title: strip_priority_prefix(&finding.title).to_string(),
            body: finding.body,
            file: Some(relative_to_repo(
                &finding.code_location.absolute_file_path,
                repo_root,
            )),
            line_start: Some(finding.code_location.line_range.start),
            line_end: Some(finding.code_location.line_range.end),
            confidence: finding.confidence_score,
        });
    }
    let verdict = match output.overall_correctness.as_str() {
        "patch is correct" => ReviewVerdict::Correct,
        "patch is incorrect" => ReviewVerdict::NeedsChanges,
        _ => ReviewVerdict::Unknown,
    };
    Ok(ReviewFindings {
        verdict,
        summary: output.overall_explanation,
        findings,
        reviewer,
        structured: true,
    })
}

/// Reads the rendered review text. When the text has a findings heading and
/// every bullet parses, the result is structured (verdict `needs_changes`).
/// Otherwise, including a review with no findings (whose text is only the
/// explanation), the whole text becomes the summary with no findings,
/// verdict `unknown` and `structured: false`, so nothing the reviewer wrote
/// is lost.
pub fn review_findings_from_text(
    text: &str,
    repo_root: &Path,
    reviewer: Reviewer,
) -> ReviewFindings {
    match parse_rendered_findings(text, repo_root) {
        Some((summary, findings)) => ReviewFindings {
            verdict: ReviewVerdict::NeedsChanges,
            summary,
            findings,
            reviewer,
            structured: true,
        },
        None => ReviewFindings {
            verdict: ReviewVerdict::Unknown,
            summary: text.trim().to_string(),
            findings: Vec::new(),
            reviewer,
            structured: false,
        },
    }
}

fn parse_rendered_findings(text: &str, repo_root: &Path) -> Option<(String, Vec<ReviewFinding>)> {
    let (heading_at, heading) = FINDINGS_HEADINGS
        .iter()
        .filter_map(|heading| text.find(heading).map(|at| (at, *heading)))
        .min_by_key(|(at, _)| *at)?;
    let summary = text[..heading_at].trim().to_string();
    let rest = &text[heading_at + heading.len()..];

    let mut findings: Vec<ReviewFinding> = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();
    for line in rest.lines() {
        if let Some(header) = line.strip_prefix("- ") {
            if let Some(previous) = findings.last_mut() {
                previous.body = join_body(&body_lines);
            }
            body_lines.clear();
            let (severity, title, file, line_start, line_end) = parse_bullet_header(header)?;
            findings.push(ReviewFinding {
                id: finding_id(findings.len()),
                severity,
                title: title.to_string(),
                body: String::new(),
                file: Some(relative_to_repo(Path::new(file), repo_root)),
                line_start: Some(line_start),
                line_end: Some(line_end),
                confidence: None,
            });
        } else if line.trim().is_empty() {
            body_lines.push("");
        } else if let Some(body) = line.strip_prefix(BODY_INDENT) {
            findings.last()?;
            body_lines.push(body);
        } else {
            // Unindented text after the heading is not part of the format.
            return None;
        }
    }
    let last = findings.last_mut()?;
    last.body = join_body(&body_lines);
    Some((summary, findings))
}

/// Parses `[P<n>] <title> — /<path>:<start>-<end>` (the part after `- `).
fn parse_bullet_header(header: &str) -> Option<(ReviewSeverity, &str, &str, u32, u32)> {
    let rest = header.strip_prefix("[P")?;
    let mut chars = rest.chars();
    let digit = chars.next()?.to_digit(10)?;
    let severity = ReviewSeverity::from_priority(u8::try_from(digit).ok()?)?;
    let rest = chars.as_str().strip_prefix("] ")?;
    let (title, location) = rest.rsplit_once(TITLE_LOCATION_SEPARATOR)?;
    if title.is_empty() || !location.starts_with('/') {
        return None;
    }
    let (file, range) = location.rsplit_once(':')?;
    if file.len() < 2 {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    Some((
        severity,
        title,
        file,
        parse_line_number(start)?,
        parse_line_number(end)?,
    ))
}

fn parse_line_number(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn join_body(lines: &[&str]) -> String {
    lines.join("\n").trim().to_string()
}

fn finding_id(index: usize) -> String {
    format!("F{}", index + 1)
}

/// Codex titles usually carry the rendered `[Pn] ` prefix; the severity is
/// stored separately.
fn strip_priority_prefix(title: &str) -> &str {
    let Some(rest) = title.strip_prefix("[P") else {
        return title;
    };
    let mut chars = rest.chars();
    match (chars.next(), chars.as_str().strip_prefix("] ")) {
        (Some(digit), Some(stripped)) if digit.is_ascii_digit() => stripped,
        _ => title,
    }
}

/// `path` relative to `repo_root` when it is under it, else `path` as given.
/// Tries the paths as written first, then canonicalised (Codex reports
/// canonical paths, e.g. `/private/tmp/...` for `/tmp/...`).
pub fn relative_to_repo(path: &Path, repo_root: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(repo_root) {
        return relative.to_path_buf();
    }
    let canonical_root = repo_root.canonicalize().ok();
    let canonical_path = path.canonicalize().ok();
    for candidate in [Some(path), canonical_path.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(root) = canonical_root.as_deref() {
            if let Ok(relative) = candidate.strip_prefix(root) {
                return relative.to_path_buf();
            }
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
    const SCRATCH_REPO: &str = "/scratch/repo";

    fn fixture(name: &str) -> String {
        let path = Path::new(FIXTURES).join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
    }

    fn codex() -> Reviewer {
        Reviewer {
            harness: "codex".to_string(),
            model: None,
            conversation_id: Some("thread-1".to_string()),
        }
    }

    fn summary_of(findings: &ReviewFindings) -> Vec<(String, ReviewSeverity, PathBuf, u32, u32)> {
        findings
            .findings
            .iter()
            .map(|finding| {
                (
                    finding.id.clone(),
                    finding.severity,
                    finding.file.clone().unwrap(),
                    finding.line_start.unwrap(),
                    finding.line_end.unwrap(),
                )
            })
            .collect()
    }

    fn expected(rows: &[(&str, u32)]) -> Vec<(String, ReviewSeverity, PathBuf, u32, u32)> {
        rows.iter()
            .enumerate()
            .map(|(index, (file, line))| {
                (
                    format!("F{}", index + 1),
                    ReviewSeverity::P2,
                    PathBuf::from(file),
                    *line,
                    *line,
                )
            })
            .collect()
    }

    #[test]
    fn rollout_uncommitted_has_one_finding() {
        let review = review_findings_from_rollout(
            &fixture("review-uncommitted.rollout.jsonl"),
            Path::new(SCRATCH_REPO),
            codex(),
        )
        .unwrap();
        assert!(review.structured);
        assert_eq!(review.verdict, ReviewVerdict::NeedsChanges);
        assert_eq!(summary_of(&review), expected(&[("stats.py", 18)]));
        let finding = &review.findings[0];
        assert_eq!(finding.title, "Return None when no user matches");
        assert!(finding.body.starts_with("When `users` is empty"));
        assert_eq!(finding.confidence, Some(1.0));
        assert!(review
            .summary
            .starts_with("The change introduces a confirmed regression"));
        assert_eq!(review.reviewer, codex());
    }

    #[test]
    fn rollout_base_has_three_findings() {
        let review = review_findings_from_rollout(
            &fixture("review-base.rollout.jsonl"),
            Path::new(SCRATCH_REPO),
            codex(),
        )
        .unwrap();
        assert!(review.structured);
        assert_eq!(review.verdict, ReviewVerdict::NeedsChanges);
        assert_eq!(
            summary_of(&review),
            expected(&[("stats.py", 9), ("stats.py", 18), ("stats.py", 23)])
        );
        assert_eq!(
            review.findings[0].title,
            "Include the final complete moving-average window"
        );
    }

    #[test]
    fn rollout_commit_has_two_findings() {
        let review = review_findings_from_rollout(
            &fixture("review-commit.rollout.jsonl"),
            Path::new(SCRATCH_REPO),
            codex(),
        )
        .unwrap();
        assert!(review.structured);
        assert_eq!(review.verdict, ReviewVerdict::NeedsChanges);
        assert_eq!(
            summary_of(&review),
            expected(&[("stats.py", 9), ("stats.py", 25)])
        );
        assert_eq!(
            review.findings[1].title,
            "Correct the last_n slice boundary"
        );
    }

    #[test]
    fn rollout_clean_is_correct_with_no_findings() {
        let review = review_findings_from_rollout(
            &fixture("review-clean.rollout.jsonl"),
            Path::new(SCRATCH_REPO),
            codex(),
        )
        .unwrap();
        assert!(review.structured);
        assert_eq!(review.verdict, ReviewVerdict::Correct);
        assert!(review.findings.is_empty());
        assert!(review
            .summary
            .starts_with("The only change expands the module docstring"));
    }

    #[test]
    fn rollout_paths_outside_the_repo_stay_absolute() {
        let review = review_findings_from_rollout(
            &fixture("review-uncommitted.rollout.jsonl"),
            Path::new("/elsewhere/repo"),
            codex(),
        )
        .unwrap();
        assert_eq!(
            review.findings[0].file.as_deref(),
            Some(Path::new("/scratch/repo/stats.py"))
        );
    }

    #[test]
    fn rollout_without_a_review_item_is_an_error() {
        let error = review_findings_from_rollout(
            &fixture("review-uncommitted.jsonl"),
            Path::new(SCRATCH_REPO),
            codex(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no ExitedReviewMode item"));
        let error =
            review_findings_from_rollout("not json", Path::new(SCRATCH_REPO), codex()).unwrap_err();
        assert!(error.to_string().contains("line 1 is not JSON"));
    }

    #[test]
    fn rollout_rejects_a_priority_outside_zero_to_three() {
        let rollout =
            fixture("review-uncommitted.rollout.jsonl").replace("\"priority\":2", "\"priority\":7");
        assert!(rollout.contains("\"priority\":7"));
        let error =
            review_findings_from_rollout(&rollout, Path::new(SCRATCH_REPO), codex()).unwrap_err();
        assert!(error.to_string().contains("priority 7"));
    }

    #[test]
    fn text_uncommitted_has_one_finding_under_review_comment() {
        let review = review_findings_from_text(
            &fixture("review-uncommitted.last.md"),
            Path::new(SCRATCH_REPO),
            codex(),
        );
        assert!(review.structured);
        assert_eq!(review.verdict, ReviewVerdict::NeedsChanges);
        assert_eq!(summary_of(&review), expected(&[("stats.py", 18)]));
        let finding = &review.findings[0];
        assert_eq!(finding.title, "Return None when no user matches");
        assert!(finding.body.starts_with("When `users` is empty"));
        assert!(finding
            .body
            .ends_with("Guard the empty result before indexing."));
        assert_eq!(finding.confidence, None);
        assert_eq!(
            review.summary,
            "The change introduces a confirmed regression: unsuccessful user lookups now raise IndexError rather than returning None."
        );
    }

    #[test]
    fn text_base_has_three_findings_under_full_review_comments() {
        let review = review_findings_from_text(
            &fixture("review-base.last.md"),
            Path::new(SCRATCH_REPO),
            codex(),
        );
        assert!(review.structured);
        assert_eq!(
            summary_of(&review),
            expected(&[("stats.py", 9), ("stats.py", 18), ("stats.py", 23)])
        );
        for finding in &review.findings {
            assert!(!finding.body.is_empty());
            assert!(!finding.body.contains("- [P"));
            assert!(!finding.body.starts_with(' '));
        }
        assert_eq!(
            review.findings[2].title,
            "Correct the starting index for the last n values"
        );
        assert!(review.findings[2]
            .body
            .ends_with("clamp it to zero when necessary."));
    }

    #[test]
    fn text_commit_has_two_findings() {
        let review = review_findings_from_text(
            &fixture("review-commit.last.md"),
            Path::new(SCRATCH_REPO),
            codex(),
        );
        assert!(review.structured);
        assert_eq!(
            summary_of(&review),
            expected(&[("stats.py", 9), ("stats.py", 25)])
        );
    }

    #[test]
    fn text_and_rollout_agree_on_every_fixture_with_findings() {
        for name in ["review-uncommitted", "review-base", "review-commit"] {
            let rollout = review_findings_from_rollout(
                &fixture(&format!("{name}.rollout.jsonl")),
                Path::new(SCRATCH_REPO),
                codex(),
            )
            .unwrap();
            let text = review_findings_from_text(
                &fixture(&format!("{name}.last.md")),
                Path::new(SCRATCH_REPO),
                codex(),
            );
            assert_eq!(summary_of(&rollout), summary_of(&text), "{name}");
            for (structured, rendered) in rollout.findings.iter().zip(&text.findings) {
                assert_eq!(structured.title, rendered.title, "{name}");
                assert_eq!(structured.body, rendered.body, "{name}");
            }
        }
    }

    #[test]
    fn text_clean_falls_back_to_the_whole_text() {
        let text = fixture("review-clean.last.md");
        let review = review_findings_from_text(&text, Path::new(SCRATCH_REPO), codex());
        assert!(!review.structured);
        assert_eq!(review.verdict, ReviewVerdict::Unknown);
        assert!(review.findings.is_empty());
        assert_eq!(review.summary, text.trim());
    }

    #[test]
    fn text_with_a_malformed_bullet_falls_back_to_the_whole_text() {
        let text = fixture("review-base.last.md").replace(
            "- [P2] Return None when no user matches — /scratch/repo/stats.py:18-18",
            "- [P9] Return None when no user matches — stats.py:18",
        );
        let review = review_findings_from_text(&text, Path::new(SCRATCH_REPO), codex());
        assert!(!review.structured);
        assert!(review.findings.is_empty());
        assert_eq!(review.summary, text.trim());
    }

    #[test]
    fn bullet_headers_parse_every_priority_and_keep_dashes_in_titles() {
        for (digit, severity) in [
            ('0', ReviewSeverity::P0),
            ('1', ReviewSeverity::P1),
            ('2', ReviewSeverity::P2),
            ('3', ReviewSeverity::P3),
        ] {
            let header = format!("[P{digit}] Fix a — b — /repo/src/a b.rs:3-7");
            assert_eq!(
                parse_bullet_header(&header),
                Some((severity, "Fix a — b", "/repo/src/a b.rs", 3, 7))
            );
        }
        assert_eq!(parse_bullet_header("[P4] Bad — /repo/a.rs:1-1"), None);
        assert_eq!(parse_bullet_header("[P1] Bad — repo/a.rs:1-1"), None);
        assert_eq!(parse_bullet_header("[P1] Bad — /repo/a.rs:1"), None);
        assert_eq!(parse_bullet_header("[P1] Bad - /repo/a.rs:1-2"), None);
    }

    #[test]
    fn priority_prefix_is_stripped_only_when_present() {
        assert_eq!(strip_priority_prefix("[P1] Title"), "Title");
        assert_eq!(strip_priority_prefix("Title"), "Title");
        assert_eq!(strip_priority_prefix("[Px] Title"), "[Px] Title");
    }

    #[test]
    fn relative_to_repo_uses_canonical_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let canonical = root.canonicalize().unwrap().join("src/lib.rs");
        assert_eq!(
            relative_to_repo(&canonical, &root),
            PathBuf::from("src/lib.rs")
        );
        assert_eq!(
            relative_to_repo(&root.join("src/lib.rs"), &root),
            PathBuf::from("src/lib.rs")
        );
        assert_eq!(
            relative_to_repo(Path::new("/elsewhere/x.rs"), &root),
            PathBuf::from("/elsewhere/x.rs")
        );
    }
}
