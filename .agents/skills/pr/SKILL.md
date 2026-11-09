---
name: pr
description: "Open or update a pull request for the current branch against v6."
---
<!-- agent-kit:managed v0.2.0 sha256:6a80511c4eafde7fd99bba098db988c3fd668bf35b6b8c87d160626d51055ca6 -->
# Pull request

Push and open a PR only when the user asks.

1. Run the `pr-ready` skill. Open the PR as a draft while it reports Blocked.
2. Push the branch: `git push -u origin HEAD`.
3. Open the PR against `v6` with the repository host's tool (on GitHub:
   `gh pr create --base v6`). If the repo has a PR template, fill it in.
4. The description says what changed and why, how it was verified (the checks
   and their results), and any risk or follow-up.
5. Report the PR link.
