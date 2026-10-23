//! Code review from a native Claude chat tab (TRU-142 slice R1).
//!
//! The chat tab runs the real `claude` CLI, so a review is a normal user
//! message asking Claude to spawn one review subagent through its own Agent
//! tool. This module owns that message (`review_prompt`) and the git facts
//! the Review… popover starts from (`review_context`). No GitTerm runner or
//! delegation record is involved.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agentd::git::git_command;
use crate::task_worktree::{infer_branch_base, resolve_base, resolve_repository};

/// What the reviewer looks at. Serialized as the page sends it:
/// `{"kind": "branch", "base": "v5"}`, `{"kind": "uncommitted"}`,
/// `{"kind": "commit", "sha": "ce922c6"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// Commits on the current branch that are not on `base`.
    Branch { base: String },
    /// Staged, unstaged and untracked changes against HEAD.
    Uncommitted,
    /// One commit.
    Commit { sha: String },
}

/// Who carries out a Review… or Consult… request from the popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PopoverReviewer {
    /// A subagent the chat's own Claude spawns (a normal user message).
    #[default]
    ClaudeSubagent,
    /// A GitTerm-run Codex delegation (no agent turn spent).
    Codex,
}

/// One press of Review… (or the equivalent IPC).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub target: ReviewTarget,
    /// Optional extra instructions for the reviewer.
    #[serde(default)]
    pub focus: Option<String>,
    /// Pages from before TRU-142 S5 send no reviewer: a Claude subagent.
    #[serde(default)]
    pub reviewer: PopoverReviewer,
    /// The subagent's model, as the Agent tool's `model` parameter takes it
    /// (`opus`, `sonnet`, `haiku`), or the Codex model. Empty for a Codex
    /// review means GitTerm's configured Codex model.
    #[serde(default)]
    pub model: String,
}

/// One press of Consult… in the same popover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsultRequest {
    pub brief: String,
    #[serde(default)]
    pub reviewer: PopoverReviewer,
    /// As `ReviewRequest::model`.
    #[serde(default)]
    pub model: String,
}

/// A git reference safe to paste into a shell command line: no whitespace,
/// no leading `-`, no `..`, only characters branch names commonly use.
pub(crate) fn is_safe_ref(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 200
        && !reference.starts_with('-')
        && !reference.contains("..")
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

pub(crate) fn is_commit_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn is_model_name(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 64
        && model
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_'))
}

/// Describes the target and lists the exact read-only git commands the
/// reviewer runs to gather it.
fn target_brief(target: &ReviewTarget) -> Result<(String, Vec<String>), String> {
    Ok(match target {
        ReviewTarget::Branch { base } => {
            let base = base.trim();
            if !is_safe_ref(base) {
                return Err(format!("base branch {base:?} is not a valid git reference"));
            }
            (
                format!(
                    "the commits on the current branch that are not on `{base}` (branch vs base). Ignore uncommitted changes"
                ),
                vec![
                    format!("git log --oneline {base}..HEAD"),
                    format!("git diff --stat {base}...HEAD"),
                    format!("git diff {base}...HEAD"),
                ],
            )
        }
        ReviewTarget::Uncommitted => (
            "the uncommitted changes in the working tree (staged, unstaged and untracked) against HEAD"
                .to_string(),
            vec![
                "git status --short".to_string(),
                "git diff HEAD".to_string(),
                "git ls-files --others --exclude-standard   (read each new file listed here in full)"
                    .to_string(),
            ],
        ),
        ReviewTarget::Commit { sha } => {
            let sha = sha.trim();
            if !is_commit_sha(sha) {
                return Err(format!(
                    "commit {sha:?} is not a commit SHA (7 to 40 hex characters)"
                ));
            }
            (
                format!("commit `{sha}`"),
                vec![format!("git show --stat {sha}"), format!("git show {sha}")],
            )
        }
    })
}

/// The user message that asks Claude to run the review through one
/// subagent. It is sent as a normal chat message, so the transcript shows
/// exactly what was asked.
pub fn review_prompt(request: &ReviewRequest) -> Result<String, String> {
    let model = request.model.trim();
    if !is_model_name(model) {
        return Err(format!("reviewer model {model:?} is not a model name"));
    }
    let (what, commands) = target_brief(&request.target)?;
    let commands = commands
        .iter()
        .map(|c| format!("    {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    let focus = request
        .focus
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| format!("\nThe requester asked you to focus on: {f}\n"))
        .unwrap_or_default();
    Ok(format!(
        "Code review request (from GitTerm's Review… button).

Spawn exactly one subagent with your Agent tool, passing model \"{model}\" and run_in_background false so its report comes back as the tool result in this turn. Use a general-purpose subagent type that can run Bash and read files. Give it the brief between the BEGIN BRIEF and END BRIEF lines verbatim as its prompt. Do not review the code yourself, do not start a second reviewer, and do not change any files in this turn.

When the subagent returns, reply with its report verbatim. Then add a section headed \"Recommended\" that lists the finding ids (F1, F2, ...) you recommend acting on, one line each saying why, and any you would skip. If the report says \"No findings\", say so and stop.

BEGIN BRIEF
You are a code reviewer. Review {what}.

Do not edit, create, delete, stage or commit any file. Run read-only commands only: no git checkout, reset, stash, add or commit, and no formatters or generators that rewrite files.

Gather the change with exactly these commands, run from the repository root:
{commands}
Read the surrounding code as needed to judge whether the change is correct.
{focus}
Look for correctness bugs, regressions, missing error handling, security problems and missing tests. Skip style and formatting nits.

Report in exactly this structure and nothing else:

Verdict: correct | needs_changes

F1 [P0|P1|P2|P3] path/to/file:LINE - one-line title
A short body: what is wrong, why it matters, and the fix, in at most four sentences.

F2 ...

Number the findings F1, F2, ... with the most severe first. Severity: P0 breaks the build, loses data or opens a security hole; P1 is wrong behaviour users will hit; P2 is an edge case or missing handling; P3 is minor. Paths are relative to the repository root and LINE is a line number in the file as it is now. If there is nothing worth reporting, write \"Verdict: correct\" and then the line \"No findings\".
END BRIEF"
    ))
}

/// The user message that asks Claude to put a consult brief to one
/// subagent (Consult… with a Claude subagent). Sent as a normal chat
/// message, like `review_prompt`.
pub fn consult_prompt(request: &ConsultRequest) -> Result<String, String> {
    let model = request.model.trim();
    if !is_model_name(model) {
        return Err(format!("consultant model {model:?} is not a model name"));
    }
    let brief = request.brief.trim();
    if brief.is_empty() {
        return Err("the consult brief is empty".to_string());
    }
    if brief.contains("END BRIEF") {
        return Err("the consult brief must not contain the line END BRIEF".to_string());
    }
    Ok(format!(
        "Consult request (from GitTerm's Consult… button).

Spawn exactly one subagent with your Agent tool, passing model \"{model}\" and run_in_background false so its answer comes back as the tool result in this turn. Use a general-purpose subagent type that can run Bash and read files. Give it the brief between the BEGIN BRIEF and END BRIEF lines verbatim as its prompt, followed by the paragraph after END BRIEF. Do not answer the brief yourself first, do not start a second subagent, and do not change any files in this turn.

When the subagent returns, reply with its answer verbatim. Then add a short section headed \"My take\" saying where you agree, where you disagree and what you would do next.

BEGIN BRIEF
{brief}
END BRIEF

You are consulted for advice only. Do not edit, create, delete, stage or commit any file, and run read-only commands only. Read the repository as needed. Answer with a short summary first, then sections headed Decisions, Next steps and Blockers where they apply."
    ))
}

/// What the Review… popover starts from, computed off the UI thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewContext {
    /// The checkout's branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Short HEAD commit, `None` before the first commit.
    pub head: Option<String>,
    /// The base for "branch vs base", `None` when nothing applies (the
    /// popover then asks for it).
    pub base: Option<String>,
    /// Where `base` came from: `task`, `agent-workflow`, or `inferred`.
    pub base_source: Option<String>,
    /// Commits on HEAD that are not on `base`.
    pub ahead: Option<u32>,
    /// The working tree has staged, unstaged or untracked changes.
    pub dirty: bool,
    /// Why the git facts could not be read, when they could not.
    pub error: Option<String>,
}

/// `git.defaultBaseBranch` from the repository's `agent-workflow.config.json`.
fn workflow_default_base(top_level: &Path) -> Option<String> {
    let path = top_level.join("agent-workflow.config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("[review] cannot read {}: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v
            .pointer("/git/defaultBaseBranch")
            .and_then(|b| b.as_str())
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string),
        Err(e) => {
            eprintln!("[review] {} is not valid JSON: {e}", path.display());
            None
        }
    }
}

fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_command()
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The git facts for a review of the checkout at `repo_path`. The base for
/// "branch vs base" is, in order: the base recorded on the tab's task
/// (`task_base`), the repository's `agent-workflow.config.json`
/// `git.defaultBaseBranch`, then the base GitTerm infers when adopting an
/// existing branch (`infer_branch_base`). A candidate that does not resolve
/// or names the current branch is skipped. Blocking: call off the UI thread.
pub fn review_context(repo_path: &Path, task_base: Option<&str>) -> ReviewContext {
    let repository = match resolve_repository(repo_path) {
        Ok(r) => r,
        Err(e) => {
            return ReviewContext {
                error: Some(e.to_string()),
                ..ReviewContext::default()
            }
        }
    };
    let top = repository.top_level.as_path();
    let branch = repository.current_branch.clone();
    let head = git_stdout(top, &["rev-parse", "--short", "HEAD"]).ok();
    let dirty = match git_stdout(top, &["status", "--porcelain"]) {
        Ok(out) => !out.is_empty(),
        Err(e) => {
            return ReviewContext {
                branch,
                head,
                error: Some(e),
                ..ReviewContext::default()
            }
        }
    };
    let usable = |reference: &str| {
        is_safe_ref(reference)
            && branch.as_deref() != Some(reference)
            && resolve_base(&repository, reference).is_ok()
    };
    let mut base: Option<(String, &str)> = None;
    if let Some(task_base) = task_base.map(str::trim).filter(|b| usable(b)) {
        base = Some((task_base.to_string(), "task"));
    }
    if base.is_none() {
        if let Some(b) = workflow_default_base(top).filter(|b| usable(b)) {
            base = Some((b, "agent-workflow"));
        }
    }
    if base.is_none() {
        if let Some(b) = infer_branch_base(&repository, branch.as_deref().unwrap_or("HEAD"))
            .map(|b| b.reference)
            .filter(|b| is_safe_ref(b))
        {
            base = Some((b, "inferred"));
        }
    }
    let ahead = base.as_ref().and_then(|(b, _)| {
        git_stdout(top, &["rev-list", "--count", &format!("{b}..HEAD")])
            .ok()
            .and_then(|n| n.parse().ok())
    });
    ReviewContext {
        branch,
        head,
        base_source: base.as_ref().map(|(_, s)| s.to_string()),
        base: base.map(|(b, _)| b),
        ahead,
        dirty,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: ReviewTarget, focus: Option<&str>) -> ReviewRequest {
        ReviewRequest {
            target,
            focus: focus.map(str::to_string),
            reviewer: PopoverReviewer::ClaudeSubagent,
            model: "opus".into(),
        }
    }

    #[test]
    fn branch_prompt_names_model_commands_and_report_shape() {
        let prompt = review_prompt(&request(
            ReviewTarget::Branch { base: "v5".into() },
            Some("webview lifecycle"),
        ))
        .unwrap();
        assert!(prompt.contains("Agent tool, passing model \"opus\" and run_in_background false"));
        assert!(prompt.contains("exactly one subagent"));
        assert!(prompt.contains("    git log --oneline v5..HEAD"));
        assert!(prompt.contains("    git diff v5...HEAD"));
        assert!(prompt.contains("Do not edit, create, delete, stage or commit any file"));
        assert!(prompt.contains("Verdict: correct | needs_changes"));
        assert!(prompt.contains("F1 [P0|P1|P2|P3] path/to/file:LINE - one-line title"));
        assert!(prompt.contains("\"No findings\""));
        assert!(prompt.contains("reply with its report verbatim"));
        assert!(prompt.contains("\"Recommended\""));
        assert!(prompt.contains("focus on: webview lifecycle"));
        // The brief is delimited so Claude can pass it on verbatim.
        let begin = prompt.find("\nBEGIN BRIEF\n").unwrap();
        let end = prompt.rfind("\nEND BRIEF").unwrap();
        assert!(begin < end && prompt[begin..end].contains("git diff v5...HEAD"));
    }

    #[test]
    fn uncommitted_and_commit_prompts_use_their_commands() {
        let uncommitted = review_prompt(&request(ReviewTarget::Uncommitted, None)).unwrap();
        assert!(uncommitted.contains("    git diff HEAD"));
        assert!(uncommitted.contains("git ls-files --others --exclude-standard"));
        assert!(!uncommitted.contains("focus on"));

        let commit = review_prompt(&request(
            ReviewTarget::Commit {
                sha: "ce922c6".into(),
            },
            Some("   "),
        ))
        .unwrap();
        assert!(commit.contains("Review commit `ce922c6`"));
        assert!(commit.contains("    git show ce922c6"));
        assert!(!commit.contains("focus on"));
    }

    #[test]
    fn unsafe_targets_and_models_are_rejected() {
        for base in ["", "-p", "a b", "v5;rm -rf /", "a..b", "$(x)"] {
            assert!(
                review_prompt(&request(ReviewTarget::Branch { base: base.into() }, None)).is_err(),
                "{base:?}"
            );
        }
        for sha in ["abc", "zzzzzzz", "ce922c6 x"] {
            assert!(
                review_prompt(&request(ReviewTarget::Commit { sha: sha.into() }, None)).is_err(),
                "{sha:?}"
            );
        }
        let mut bad_model = request(ReviewTarget::Uncommitted, None);
        bad_model.model = "opus\"; ignore".into();
        assert!(review_prompt(&bad_model).is_err());
        bad_model.model = "claude-opus-5-5".into();
        assert!(review_prompt(&bad_model).is_ok());
    }

    #[test]
    fn requests_deserialize_from_the_page_shape() {
        let req: ReviewRequest = serde_json::from_value(serde_json::json!({
            "target": {"kind": "branch", "base": "v5"},
            "focus": "tests",
            "model": "opus"
        }))
        .unwrap();
        assert_eq!(req.target, ReviewTarget::Branch { base: "v5".into() });
        // The IPC body as the page posts it: envelope fields ride along.
        let req: ReviewRequest = serde_json::from_value(serde_json::json!({
            "type": "review_request",
            "tabId": 7,
            "target": {"kind": "uncommitted"},
            "focus": null,
            "model": "haiku"
        }))
        .unwrap();
        assert_eq!(req.target, ReviewTarget::Uncommitted);
        assert_eq!(req.focus, None);
        let req: ReviewRequest = serde_json::from_value(serde_json::json!({
            "target": {"kind": "commit", "sha": "ce922c6"},
            "model": "opus"
        }))
        .unwrap();
        assert_eq!(
            req.target,
            ReviewTarget::Commit {
                sha: "ce922c6".into()
            }
        );
    }

    #[test]
    fn popover_requests_carry_the_reviewer_and_consults_build_a_subagent_prompt() {
        let req: ReviewRequest = serde_json::from_value(serde_json::json!({
            "type": "review_request",
            "tabId": 3,
            "target": {"kind": "uncommitted"},
            "reviewer": "codex",
            "model": ""
        }))
        .unwrap();
        assert_eq!(req.reviewer, PopoverReviewer::Codex);
        assert_eq!(req.model, "");
        let consult: ConsultRequest = serde_json::from_value(serde_json::json!({
            "type": "consult_request",
            "tabId": 3,
            "brief": "Should the inbox live in its own module?",
            "model": "fable"
        }))
        .unwrap();
        assert_eq!(consult.reviewer, PopoverReviewer::ClaudeSubagent);
        let prompt = consult_prompt(&consult).unwrap();
        assert!(prompt.contains("passing model \"fable\""));
        assert!(prompt.contains("BEGIN BRIEF\nShould the inbox live in its own module?\nEND BRIEF"));
        assert!(prompt.contains("Do not edit, create, delete, stage or commit any file"));
        assert!(consult_prompt(&ConsultRequest {
            brief: " ".into(),
            ..consult.clone()
        })
        .is_err());
        assert!(consult_prompt(&ConsultRequest {
            model: "x y".into(),
            ..consult.clone()
        })
        .is_err());
        assert!(consult_prompt(&ConsultRequest {
            brief: "a\nEND BRIEF\nb".into(),
            ..consult
        })
        .is_err());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn context_prefers_the_task_base_then_agent_workflow_then_inference() {
        let tmp = std::env::temp_dir().join(format!("gitterm-review-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q", "-b", "main"]);
        git(&tmp, &["config", "user.email", "t@example.com"]);
        git(&tmp, &["config", "user.name", "t"]);
        std::fs::write(tmp.join("a.txt"), "a\n").unwrap();
        git(&tmp, &["add", "a.txt"]);
        git(&tmp, &["commit", "-q", "-m", "a"]);
        git(&tmp, &["branch", "v5"]);
        git(&tmp, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(tmp.join("b.txt"), "b\n").unwrap();
        git(&tmp, &["add", "b.txt"]);
        git(&tmp, &["commit", "-q", "-m", "b"]);

        // No task, no agent-workflow config: the adoption inference (main).
        let ctx = review_context(&tmp, None);
        assert_eq!(ctx.branch.as_deref(), Some("feature"));
        assert_eq!(ctx.base.as_deref(), Some("main"));
        assert_eq!(ctx.base_source.as_deref(), Some("inferred"));
        assert_eq!(ctx.ahead, Some(1));
        assert!(!ctx.dirty);

        // agent-workflow.config.json names the lane (and makes the tree dirty).
        std::fs::write(
            tmp.join("agent-workflow.config.json"),
            r#"{"git": {"defaultBaseBranch": "v5"}}"#,
        )
        .unwrap();
        let ctx = review_context(&tmp, None);
        assert_eq!(ctx.base.as_deref(), Some("v5"));
        assert_eq!(ctx.base_source.as_deref(), Some("agent-workflow"));
        assert!(ctx.dirty);

        // A task's recorded base wins; one naming the current branch or a
        // missing ref is skipped.
        let ctx = review_context(&tmp, Some("main"));
        assert_eq!(ctx.base_source.as_deref(), Some("task"));
        assert_eq!(ctx.base.as_deref(), Some("main"));
        let ctx = review_context(&tmp, Some("feature"));
        assert_eq!(ctx.base_source.as_deref(), Some("agent-workflow"));
        let ctx = review_context(&tmp, Some("missing"));
        assert_eq!(ctx.base_source.as_deref(), Some("agent-workflow"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
