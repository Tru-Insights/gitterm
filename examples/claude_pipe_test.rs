// Hour-1 spike: verify that Rust can drive
// `claude --print --output-format stream-json --verbose`
// as a subprocess, streaming JSON events from stdout and parsing them cleanly.
//
// Multi-turn / interactive input is deliberately out of scope here —
// that's hour 2. This one just proves the output pipe and event parsing.
//
// Usage:
//   cargo run --example claude_pipe_test -- "your prompt here"
//
// Optional env:
//   CLAUDE_MODEL=haiku|sonnet|opus  (default: haiku)

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .ok_or("usage: claude_pipe_test \"<prompt>\"")?;
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "haiku".to_string());

    eprintln!("[spike] model={} prompt={:?}", model, prompt);

    let mut child = Command::new("claude")
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            &model,
            &prompt,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = child.stdout.take().ok_or("child stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();

    let mut event_count = 0usize;
    let mut assistant_text = String::new();

    while let Some(line) = lines.next_line().await? {
        event_count += 1;
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => summarize_event(&v, &mut assistant_text),
            Err(e) => eprintln!("[spike] parse error: {} | line: {}", e, line),
        }
    }

    let status = child.wait().await?;
    eprintln!("[spike] events={} exit={}", event_count, status);
    eprintln!("[spike] assistant text: {:?}", assistant_text);
    Ok(())
}

fn summarize_event(v: &serde_json::Value, assistant_text: &mut String) {
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("?");
    let subtype = v.get("subtype").and_then(|t| t.as_str());

    match event_type {
        "assistant" => {
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for block in content {
                    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                    match block_type {
                        "text" => {
                            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            println!("  [text] {}", text);
                            assistant_text.push_str(text);
                        }
                        "thinking" => {
                            let thinking =
                                block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            let preview = thinking.chars().take(80).collect::<String>();
                            println!("  [thinking] {}...", preview);
                        }
                        "tool_use" => {
                            let name = block.get("name").and_then(|t| t.as_str()).unwrap_or("?");
                            println!("  [tool_use] {}", name);
                        }
                        other => println!("  [assistant block: {}]", other),
                    }
                }
            }
        }
        "user" => {
            // Tool results come back as user messages in multi-turn.
            println!("  [user/tool_result]");
        }
        "result" => {
            let cost = v
                .get("total_cost_usd")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0);
            let duration = v.get("duration_ms").and_then(|d| d.as_i64()).unwrap_or(0);
            let stop = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("?");
            println!(
                "[result] stop={} duration={}ms cost=${:.4}",
                stop, duration, cost
            );
        }
        "system" => {
            println!("[system:{}]", subtype.unwrap_or(""));
        }
        "rate_limit_event" => {
            println!("[rate_limit]");
        }
        other => {
            println!("[{}]", other);
        }
    }
}
