---
name: checkpoint
description: "Read or write the session checkpoint so a later session can resume the work."
---
<!-- agent-kit:managed v0.2.0 sha256:1c30e92f9fb872857ac30f9549ed46613abf764c64028897c12672c70c696fad -->
# Checkpoint

## Which file

If the session's instructions name a workspace checkpoint (a line starting
`- Workspace checkpoint:`), that file is the checkpoint: read and write it
there, even though the session runs in this repo. Otherwise the checkpoint is
`.agents/checkpoint.md` in this repo.

Why: repos are shared between workspaces, so a repo checkpoint cannot say whose
work it is.

## Resume

At the start of a session, or after context was compacted, read the checkpoint
if it exists. It is tactical context: the user's current request and
`AGENTS.md` win where they conflict.

## Write

Write the checkpoint only when asked to checkpoint or save progress. Overwrite
it with a short note a new session can read in a minute:

- **Branch** and whether there are uncommitted changes.
- **Just did**: the last meaningful step.
- **Next**: the immediate next action, with files and commands.
- **Open questions or blockers**.
