#!/usr/bin/env python3
"""List recent agent-session transcripts for a working directory.

Scans Codex, Claude Code, and pi session stores and prints the newest
transcripts whose recorded cwd matches, newest first, as:

    <harness>\t<mtime ISO>\t<path>

Usage: find-sessions.py [--cwd DIR] [--limit N]
"""

import argparse
import datetime
import json
import sys
from pathlib import Path

HOME = Path.home()


def first_json_line(path: Path):
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            line = fh.readline()
        return json.loads(line) if line.strip() else None
    except (OSError, json.JSONDecodeError):
        return None


def codex_sessions(cwd: str):
    """Codex rollouts record cwd in the session_meta payload. Top-level
    sessions have a string source ("cli", "exec"); subagent rollouts
    (guardian/review threads) have a dict source and are noise here."""
    for path in (HOME / ".codex" / "sessions").glob("*/*/*/rollout-*.jsonl"):
        meta = first_json_line(path)
        if not meta or meta.get("type") != "session_meta":
            continue
        payload = meta.get("payload", {})
        if payload.get("cwd") != cwd or isinstance(payload.get("source"), dict):
            continue
        yield "codex", path


def claude_sessions(cwd: str):
    """Claude Code escapes both '/' and '.' to '-' in the project dir name."""
    escaped = "".join("-" if ch in "/." else ch for ch in cwd)
    yield from (
        ("claude", path)
        for path in (HOME / ".claude" / "projects" / escaped).glob("*.jsonl")
    )


def pi_sessions(cwd: str):
    """pi wraps the cwd (slashes to dashes, dots kept) in leading/trailing
    dashes for its per-project session directory."""
    escaped = "-" + cwd.replace("/", "-") + "--"
    yield from (
        ("pi", path)
        for path in (HOME / ".pi" / "agent" / "sessions" / escaped).glob("*.jsonl")
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cwd", default=str(Path.cwd()), help="project directory to match")
    parser.add_argument("--limit", type=int, default=8, help="max sessions to print")
    args = parser.parse_args()

    cwd = str(Path(args.cwd).resolve())
    found = [
        (path.stat().st_mtime, harness, path)
        for source in (codex_sessions, claude_sessions, pi_sessions)
        for harness, path in source(cwd)
    ]
    if not found:
        print(f"no sessions found for cwd {cwd}", file=sys.stderr)
        return 1
    for mtime, harness, path in sorted(found, reverse=True)[: args.limit]:
        stamp = datetime.datetime.fromtimestamp(mtime).isoformat(timespec="seconds")
        print(f"{harness}\t{stamp}\t{path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
