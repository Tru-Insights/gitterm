### gitterm-v2 PR-Readiness Notes

#### Pre-flight: GitHub push access

Before pushing, check `gh auth status` and use a GitHub account that has push
access to the repository's organization.

#### Pre-flight: iced_term_fork sync

The GitHub workflows clone `Tru-Insights/iced_term` master fresh. If
you've made local commits to `../iced_term_fork` that gitterm depends on, push
them first or CI fails with
`no method named X found`-style errors:

```bash
cd ../iced_term_fork
git log @{u}..HEAD --oneline   # must be empty before triggering CI
git push origin master          # if it's not
```

See `BUILD.md` for full cross-platform build context.

#### Full Verification Set

The skill's checks above are format, lint, test and build. The actual
command set CI runs (mirror locally for parity):

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo clippy --features excalidraw -- -D warnings
cargo test
cargo test --features excalidraw
cargo build --release
```

The pre-commit hook (`.githooks/pre-commit`) runs Clippy, formatting, and
`cargo test`; see the hook for the exact commands. CI is still the authoritative
gate.

#### Linear workspace

Issues go to the `TRU` team in the `truinsights` Linear workspace. Pass the
workspace explicitly when using the Linear CLI:

```bash
linear --workspace truinsights issue create --team TRU \
  --title "..." --description-file <path> \
  --assignee self --no-interactive
```

#### Branch + PR Conventions

Branches carry the issue key. The configured default base is `v6`. PRs default
to **draft**; flip to ready only after this skill reports **Ready**. Never
merge without the user's instruction.
