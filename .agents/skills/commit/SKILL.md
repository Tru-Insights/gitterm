---
name: commit
description: "Commit the current work as focused commits with clear messages."
---
<!-- agent-kit:managed v0.2.0 sha256:db09bd7017ea361c9ea44bf649489aa384c7e8a5cf2c29b90c8fffc506e7e1ff -->
# Commit

Commit only when the user asks.

1. Check `git status` and the diff. If on `v6`, create a branch first.
2. Run the checks:

```sh
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
cargo build
```

   Do not commit on a failing check unless the user says to; then say which
   check failed in the message body.
3. Stage the files for one change by name. Do not stage unrelated files,
   secrets or build output.
4. Write the message: a short imperative subject, then a body saying why when
   the diff does not show it. The message must contain an issue key matching `TRU-[0-9]+`.
   Take it from the branch name or the task; if neither has one, ask the user.
5. Commit and report the commit hash and subject.
