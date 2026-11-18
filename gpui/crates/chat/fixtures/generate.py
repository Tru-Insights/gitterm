#!/usr/bin/env python3
"""Deterministic SYNTHETIC examples, derived from public tests/fixtures shapes.
No home directory or real transcript access. Run from any working directory.
"""
import json
from pathlib import Path

HERE = Path(__file__).parent


def event(kind, data):
    return {"type": "harness", "data": {"type": kind, "data": data}}


def write(name, entries):
    with (HERE / name).open("w") as out:
        for i, entry in enumerate(entries):
            out.write(json.dumps({"at_ms": i * 120, "entry": entry}, ensure_ascii=False) + "\n")


def tool(identifier, name, value):
    return event("item_started", {"id": identifier, "kind": {"type": "tool_call", "name": name, "input": value}})


def done(identifier, text="Done", error=False):
    return event("item_completed", {"id": identifier, "output": text, "is_error": error})


def permission(identifier):
    return event("runtime_request", {"request_id": identifier, "tool_use_id": "shell", "kind": {"type": "permission", "tool_name": "Bash", "input": {"command": "echo synthetic"}, "suggestions": [{"optionId": "allow_once", "name": "Allow once"}, {"optionId": "deny", "name": "Deny"}], "decision_reason": "Command execution requires approval"}})


def question(identifier):
    return event("runtime_request", {"request_id": identifier, "tool_use_id": "ask", "kind": {"type": "question", "questions": [{"question": "Which retry policy?", "multiSelect": False, "options": [{"label": "Exponential", "description": "Increase the delay"}, {"label": "Fixed", "description": "Keep a constant delay"}]}, {"question": "Which checks?", "multiSelect": True, "options": [{"label": "Unit", "description": "Fast checks"}, {"label": "Integration", "description": "Full workflow"}]}]}})


entries = [
    {"type": "user_prompt", "data": "Add exponential backoff to the uploader. Keep the code readable."},
    event("turn_started", {"session_id": "synthetic-session", "model": "synthetic-model"}),
    event("thinking_delta", "I will inspect the retry policy, then cover failure cases."),
    event("text_delta", "## Retry policy\n\nThe uploader can use **bounded backoff**.\n\n| Attempt | Delay |\n| --- | --- |\n| 1 | 100 ms |\n| 2 | 200 ms |\n\n```rust\nfn delay(attempt: u32) -> u64 {\n    100 * 2_u64.pow(attempt.min(4))\n}\n```\n\n"),
    tool("shell", "Bash", {}),
    event("item_input_delta", {"id": "shell", "partial_json": '{"command":'}),
    event("item_input_delta", {"id": "shell", "partial_json": '"echo synthetic"}'}),
    done("shell", "synthetic"),
    tool("partial", "Bash", {}),
    event("item_input_delta", {"id": "partial", "partial_json": '{"command":"cargo'}),
    tool("edit", "Edit", {"file_path": "src/retry.rs", "old_string": "let delay = 100;", "new_string": "let delay = delay(attempt);"}),
    done("edit"),
    tool("multi", "MultiEdit", {"file_path": "src/retry.rs", "edits": [{"old_string": "attempt += 1;", "new_string": "attempt = attempt.saturating_add(1);"}, {"old_string": "retry();", "new_string": "retry(delay);"}]}),
    done("multi"),
    tool("write", "Write", {"file_path": "docs/retry.md", "content": "# Retry\n\nUse bounded exponential backoff.\n"}),
    done("write"),
    tool("failed", "Bash", {"command": "synthetic-check"}),
    done("failed", "Synthetic check failed: missing fixture", True),
    permission("p-pending"), permission("p-answered"),
    {"type": "display_decision", "data": {"request_id": "p-answered", "state": "answered", "detail": "Allow once"}},
    event("runtime_request_resolved", {"request_id": "p-answered"}),
    permission("p-denied"),
    {"type": "display_decision", "data": {"request_id": "p-denied", "state": "denied", "detail": "Keep the fixture read-only"}},
    question("q-pending"), question("q-answered"),
    {"type": "display_decision", "data": {"request_id": "q-answered", "state": "answered", "detail": '{"Which retry policy?":"Exponential","Which checks?":"Unit, Integration"}'}},
    question("q-denied"),
    {"type": "display_decision", "data": {"request_id": "q-denied", "state": "denied", "detail": "Dismissed"}},
    tool("worker", "Agent", {"description": "Review retry bounds"}),
    event("subagent_event", {"parent_tool_use_id": "worker", "event": {"type": "turn_started", "data": {"session_id": None, "model": "synthetic-worker"}}}),
    event("subagent_event", {"parent_tool_use_id": "worker", "event": {"type": "text_delta", "data": "The cap prevents unbounded delays."}}),
    event("subagent_event", {"parent_tool_use_id": "worker", "event": {"type": "turn_completed", "data": {"status": {"state": "completed"}, "usage": {"input_tokens": 30, "output_tokens": 12}, "cost_usd": None}}}),
    done("worker", "Review complete"),
    {"type": "display_delegation", "data": {"id": "synthetic-delegation", "title": "Retry review", "state": "done", "body": "Checked bounded delays and overflow handling."}},
    event("error", "Synthetic recoverable error: fixture connection closed"),
    event("text_delta", "The retry policy now caps the delay. The example above is synthetic."),
    event("turn_completed", {"status": {"state": "completed"}, "usage": {"input_tokens": 456, "output_tokens": 196}, "cost_usd": 0.0012}),
]
write("showcase.jsonl", entries)
write("long-5000.jsonl", [
    {"type": "user_prompt", "data": f"Synthetic prompt {i:04d}: inspect bounded retries."}
    if i % 2 == 0 else event("text_delta", f"Synthetic reply {i:04d}: **bounded backoff** preserves responsive behavior.\n\nSearch marker row-{i:04d}.")
    for i in range(5000)
])
