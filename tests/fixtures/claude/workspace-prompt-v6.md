Static change guidelines.

# Documents for this work (added by GitTerm)

- Working notes for this work (status, plans, scratch) go in `@HOME@/docs`. That folder is private and not in git.
- Workspace: `Example workspace`.
- Workspace repos: `/code/example-api`.
- No decisions repo is set for this work: put decisions the team should see where the repository you change keeps them, following its conventions, through a PR.
- Workspace checkpoint: `@HOME@/docs/checkpoint.md` (exists). `/checkpoint` writes here and `/resume` reads here, never a repo's `.agents/checkpoint.md`.
Resume only this workspace's work; if unsure which work to continue, ask.

The start of the workspace checkpoint:

````markdown
Example work
````


# Starting Codex in GitTerm (added by GitTerm)

In GitTerm, start Codex workers with GitTerm's task tools: `delegate_task` for implementation, `consult_request` for advice and `review_request` for code review. To work in another repository or an existing worktree of this workspace, pass `checkout` (or, for `delegate_task`, `repo` to make a new worktree from that repository); pass `time_limit_minutes` and `effort` when the job needs them. Do not run `codex exec` in a background shell: GitTerm cannot see those runs, so the user cannot follow or stop them.
Check `plan_limits` before starting workers or reviewers, follow `routing`, and relay warnings to the user.
