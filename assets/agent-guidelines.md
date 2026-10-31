# Change guidelines (added by GitTerm)

These apply to every change you make. The repository's own instructions
(AGENTS.md, CLAUDE.md) win where they conflict.

## Before you write

- Read the task and the code it touches before changing anything.
- List every place the change must reach: callers, tests, fixtures, config,
  exports. That is the scope. Extra features are not.
- Check what the change could break for users: data it could destroy or
  expose, callers that stop working.
- Never invent API endpoints, field names, response shapes, data contracts
  or tool commands. Verify them from code, docs or the command's help.

## The smallest complete change

Take the first option that fully works:

1. Does it need to exist? Skip features, options and flexibility nobody
   asked for, and name what you skipped in one line.
2. Is it already in this codebase (a helper, component, pattern)? Use it the
   way the surrounding code does.
3. Does the standard library or platform do it? Use that, unless the project
   has its own.
4. Does an installed dependency do it? Use it. Never add a dependency for a
   few lines.
5. Otherwise, write the minimum code that works.

- Be lazy about the solution, never about the change: finish every part the
  task needs, including the callers, tests and fixtures it breaks.
- No abstraction, wrapper, option, config or "for later" code nobody asked
  for. Deleting beats adding. Keep the structure, layers and conventions the
  codebase already has. Prefer focused edits over rewrites.
- Preserve behaviour unless the task requires a change.
- Bug fix: grep every caller of the function you touch, then fix the root
  cause once, in the shared code.
- Code you move or merge keeps its error handling and validation.
- Between options of equal size, take the one that is correct on edge cases.
- Comment only the why the code cannot show.
- A shortcut with a known limit gets a comment:
  `shortcut: <the limit>, <when to upgrade>`.

## Never cut

- Validation at trust boundaries.
- Error handling that prevents data loss. Do not swallow errors; surface
  them with the operation and input that failed.
- Silent fallbacks for required values: fail loudly instead.
- Security and secrets handling, and accessibility.
- Anything the user asked for.

## Done means checked

- New non-trivial logic (a branch, a loop, a parser, money, security, data
  writes, a bug fix) leaves one small test that fails when it breaks.
  Trivial changes need none.
- Run the repository's own checks with their real exit codes. Never claim
  success without that evidence; if a check cannot run, say why.
- Review your own diff before saying the work is done.
- End your reply with one or two lines: what you skipped or did not check,
  and any risk the user must know.

---
Adapted in part from ponytail (MIT, https://github.com/dietrichgebert/ponytail).
