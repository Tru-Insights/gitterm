//! Headless smoke for Jev chat ranking (TRU-141). Ranks this machine's
//! Chats index against the current directory, branch and an optional
//! search, through TypeSafe's System One. Costs a fraction of a cent.
//!
//! Usage:
//!   TYPESAFE_API_KEY=... cargo run --example jev_rank_smoke -- \
//!     [--query <text>] [--prompt <text>] [--limit N] [--candidates N]
//!
//! --limit       ranked chats to print (default 10)
//! --candidates  most recent chats offered to Jev (default 254, one call)
//!
//! Without TYPESAFE_API_KEY it prints how to run it and exits 0 without
//! reading the index or touching the network.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use gitterm::chats;
use gitterm::jev::{self, ChatCandidate, JevClient, RankContext};

struct Args {
    query: Option<String>,
    prompt: Option<String>,
    limit: usize,
    candidates: usize,
}

fn usage() -> &'static str {
    "usage: TYPESAFE_API_KEY=<key> cargo run --example jev_rank_smoke -- \
     [--query <text>] [--prompt <text>] [--limit N] [--candidates N]"
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        query: None,
        prompt: None,
        limit: 10,
        candidates: jev::CHATS_PER_CALL,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--query" => args.query = Some(value()?),
            "--prompt" => args.prompt = Some(value()?),
            "--limit" => {
                let v = value()?;
                args.limit = v
                    .parse()
                    .map_err(|_| format!("--limit {v}: not a number"))?;
            }
            "--candidates" => {
                let v = value()?;
                args.candidates = v
                    .parse()
                    .map_err(|_| format!("--candidates {v}: not a number"))?;
            }
            "-h" | "--help" => return Err(usage().to_string()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    Ok(args)
}

fn current_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!b.is_empty() && b != "HEAD").then_some(b)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let Some(client) = JevClient::from_env() else {
        println!("{} is not set; nothing was called.", jev::API_KEY_ENV);
        println!("To run the smoke for real:");
        println!("  export {}=<your TypeSafe key>", jev::API_KEY_ENV);
        println!(
            "  cargo run --example jev_rank_smoke -- --query \"<what you are looking for>\" --limit 10"
        );
        return ExitCode::SUCCESS;
    };

    let workspace_dir: PathBuf = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("current dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ctx = RankContext {
        branch: current_branch(&workspace_dir),
        workspace_dir,
        query: args.query.clone(),
        recent_prompt: args.prompt.clone(),
    };

    let started = Instant::now();
    let index = chats::build_local_index();
    let candidates: Vec<ChatCandidate> = index
        .iter()
        .take(args.candidates)
        .map(|e| ChatCandidate {
            id: e.id.clone(),
            title: e.title.clone(),
            cwd: e.cwd.clone(),
            branch: e.branch.clone(),
            age: chats::format_age(e.mtime),
            snippet: jev::snippet_from_preview(&chats::load_preview(&e.path, e.backend).messages),
        })
        .collect();
    println!(
        "{} chats indexed, {} offered ({} ms)",
        index.len(),
        candidates.len(),
        started.elapsed().as_millis()
    );
    println!(
        "context: {} @ {} | query: {} | prompt: {}",
        ctx.workspace_dir.display(),
        ctx.branch.as_deref().unwrap_or("-"),
        ctx.query.as_deref().unwrap_or("-"),
        ctx.recent_prompt.as_deref().unwrap_or("-")
    );

    let started = Instant::now();
    let ranking = match jev::rank_chats(&client, &ctx, &candidates).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rank_chats failed: {e}");
            if e.is_down() {
                eprintln!(
                    "(TypeSafe as a whole is unusable: the Chats panel would keep recency order)"
                );
            }
            return ExitCode::FAILURE;
        }
    };
    let gate = jev::DEFAULT_CONFIDENCE_GATE;
    println!(
        "{} call(s), {} ms, {} input tokens; confidence {:.2}{}; gate {gate}: {}",
        ranking.calls,
        started.elapsed().as_millis(),
        ranking.usage.input_tokens,
        ranking.confidence,
        if ranking.chose_none {
            " (chose none)"
        } else {
            ""
        },
        if ranking.is_confident(gate) {
            "Jev order"
        } else {
            "below gate, recency order"
        }
    );
    for ranked in ranking.chats.iter().take(args.limit) {
        let Some(c) = candidates.iter().find(|c| c.id == ranked.id) else {
            eprintln!("ranked id {} is not a candidate", ranked.id);
            return ExitCode::FAILURE;
        };
        println!(
            "{:>3}. {:.3}  {:>4}  {}  [{}{}]",
            ranked.rank,
            ranked.probability,
            c.age,
            c.title,
            c.cwd.display(),
            c.branch
                .as_deref()
                .map(|b| format!(" @ {b}"))
                .unwrap_or_default()
        );
    }
    ExitCode::SUCCESS
}
