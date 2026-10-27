//! Headless smoke for the Codex runner (TRU-142 S3a). Runs the real `codex`
//! twice (one review, one short consult), so it spends Codex quota.
//!
//! Usage: cargo run --example codex_runner_smoke -- --workdir <scratch dir>
//!
//! Builds `<workdir>/codex-runner-smoke-<unix secs>/repo` with a committed
//! `stats.py` and an uncommitted change that indexes `matches[0]` without a
//! guard, reviews the uncommitted changes, then asks a two-sentence consult.
//! Exits 0 only if the review has a finding whose file is in the repo and
//! the consult summary is non-empty.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gitterm::codex_runner::{self, CodexRun, CodexRunEvent, CodexRunKind, CodexRunOutcome};
use gitterm::tasks::{ReviewTarget, ReviewTargetMode};
use tokio::sync::mpsc;

const BASE_STATS: &str = r#""""Small statistics helpers."""


def mean(values):
    """Arithmetic mean of a non-empty list."""
    return sum(values) / len(values)


def find_user(users, name):
    """Return the first user whose name matches, or None when nobody does."""
    for user in users:
        if user["name"] == name:
            return user
    return None
"#;

const CHANGED_STATS: &str = r#""""Small statistics helpers."""


def mean(values):
    """Arithmetic mean of a non-empty list."""
    return sum(values) / len(values)


def find_user(users, name):
    """Return the first user whose name matches, or None when nobody does."""
    matches = [user for user in users if user["name"] == name]
    return matches[0]
"#;

const README: &str =
    "# stats\n\nA tiny Python module of statistics helpers used to test code review tooling.\n";

#[tokio::main]
async fn main() -> ExitCode {
    let workdir = match parse_workdir() {
        Ok(workdir) => workdir,
        Err(message) => {
            eprintln!("{message}\nusage: codex_runner_smoke --workdir <scratch dir>");
            return ExitCode::from(2);
        }
    };
    match smoke(&workdir).await {
        Ok(()) => {
            println!("SMOKE OK");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("SMOKE FAILED: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parse_workdir() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--workdir"), Some(dir)) => Ok(PathBuf::from(dir)),
        _ => Err("missing --workdir".to_string()),
    }
}

async fn smoke(workdir: &Path) -> Result<(), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock before epoch: {error}"))?
        .as_secs();
    let root = workdir.join(format!("codex-runner-smoke-{stamp}"));
    let repo = root.join("repo");
    build_repo(&repo)?;
    println!("scratch repo: {}", repo.display());

    let started = Instant::now();
    let review = run_and_print(
        "review",
        CodexRun {
            kind: CodexRunKind::Review(ReviewTarget {
                mode: ReviewTargetMode::Uncommitted,
                focus: None,
            }),
            cwd: repo.clone(),
            model: None,
            log_path: root.join("logs").join("review.jsonl"),
        },
    )
    .await?;
    println!("review took {:.1}s", started.elapsed().as_secs_f64());
    let findings = review
        .result
        .findings
        .as_ref()
        .ok_or("review returned no findings record")?;
    println!(
        "review: source={:?} structured={} verdict={:?} findings={} note={:?}",
        review.findings_source,
        findings.structured,
        findings.verdict,
        findings.findings.len(),
        review.rollout_note
    );
    for finding in &findings.findings {
        println!(
            "  {} {:?} {} — {:?}:{:?}-{:?}",
            finding.id,
            finding.severity,
            finding.title,
            finding.file,
            finding.line_start,
            finding.line_end
        );
    }
    println!("reviewed: {:?}", review.result.reviewed);
    let in_repo = findings.findings.iter().any(|finding| {
        finding
            .file
            .as_ref()
            .is_some_and(|file| file.is_relative() && repo.join(file).is_file())
    });
    if !in_repo {
        return Err("no finding names a file inside the scratch repo".to_string());
    }

    let started = Instant::now();
    let consult = run_and_print(
        "consult",
        CodexRun {
            kind: CodexRunKind::Consult {
                brief: "In two sentences, what does this repository do?".to_string(),
            },
            cwd: repo.clone(),
            model: None,
            log_path: root.join("logs").join("consult.jsonl"),
        },
    )
    .await?;
    println!("consult took {:.1}s", started.elapsed().as_secs_f64());
    let handoff = consult
        .result
        .handoff
        .as_ref()
        .ok_or("consult returned no handoff")?;
    println!("consult summary: {}", handoff.summary);
    if handoff.summary.trim().is_empty() {
        return Err("consult summary is empty".to_string());
    }
    Ok(())
}

async fn run_and_print(label: &'static str, run: CodexRun) -> Result<CodexRunOutcome, String> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            match event {
                CodexRunEvent::Started { thread_id } => {
                    println!("[{label}] started thread {thread_id}")
                }
                CodexRunEvent::Activity { description } => println!("[{label}] {description}"),
                CodexRunEvent::Completed => println!("[{label}] completed"),
                CodexRunEvent::Failed { message } => println!("[{label}] failed: {message}"),
            }
        }
    });
    let log_path = run.log_path.clone();
    let result = codex_runner::run(run, sender).await;
    printer
        .await
        .map_err(|error| format!("progress printer panicked: {error}"))?;
    println!("[{label}] log: {}", log_path.display());
    result.map_err(|error| format!("{label} failed: {error}"))
}

fn build_repo(repo: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|error| format!("create {}: {error}", repo.display()))?;
    write(&repo.join("README.md"), README)?;
    write(&repo.join("stats.py"), BASE_STATS)?;
    git(repo, &["init", "-q", "-b", "main"])?;
    git(repo, &["add", "."])?;
    git(repo, &["commit", "-q", "-m", "Add stats helpers"])?;
    write(&repo.join("stats.py"), CHANGED_STATS)
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

fn git(repo: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("git");
    // A hook exports these; they would redirect git to the hook's repo.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        command.env_remove(var);
    }
    for (var, value) in [
        ("GIT_AUTHOR_NAME", "Smoke"),
        ("GIT_AUTHOR_EMAIL", "smoke@example.com"),
        ("GIT_COMMITTER_NAME", "Smoke"),
        ("GIT_COMMITTER_EMAIL", "smoke@example.com"),
    ] {
        command.env(var, value);
    }
    let output = command
        .args([
            "-c",
            "user.name=Smoke",
            "-c",
            "user.email=smoke@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("run git {args:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
