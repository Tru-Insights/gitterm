### gitterm-v2 PR-Readiness Notes

#### Pre-flight: gh active account

Pushing to `Tru-Insights/*` repos requires the **`traceyt`** (work) `gh`
account to be active. Verify before push:

```bash
gh auth status                # look for "Active account: true" next to traceyt
gh auth switch -u traceyt     # if active is traceyt-cree8 (personal)
```

A `403 Permission denied to traceyt-cree8` on push almost always means the
active gh account silently flipped back to personal — not an actual
permission problem.

Commit author metadata is independent: commits stay
`Tracey Trewin <tracey@trewin.com>` regardless of which gh account is active.
That's intentional — `gh` controls push authorization only, not authorship.

#### Pre-flight: iced_term_fork sync

All three GitHub workflows (`build.yml`, `ci.yml`, `windows-build.yml`) clone
`Tru-Insights/iced_term` master fresh. If you've made local commits to
`../iced_term_fork` that gitterm depends on, push them first or CI fails with
`no method named X found`-style errors:

```bash
cd ../iced_term_fork
git log @{u}..HEAD --oneline   # must be empty before triggering CI
git push origin master          # if it's not
```

See `BUILD.md` for full cross-platform build context.

#### Full Verification Set

`verification.beforePrReady` resolves to `check + test + build`. The actual
command set CI runs (mirror locally for parity):

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo clippy --features excalidraw -- -D warnings
cargo test
cargo test --features excalidraw
cargo build --release --features stt
```

The pre-commit hook (`.githooks/pre-commit`) runs a subset — `clippy +
fmt-check + test` — automatically when activated via
`git config core.hooksPath .githooks`. CI is still the authoritative gate.

#### Linear: TRU, not GIT

New issues default to the **`TRU`** team in the `truinsights` workspace even
though a dedicated `GIT` (Git Term) team exists. This is a deliberate user
preference. Create with:

```bash
linear --workspace truinsights issue create --team TRU \
  --title "..." --description-file <path> \
  --assignee self --no-interactive
```

Without `--workspace truinsights`, the CLI defaults to `cree8` and the
command fails or lands the issue in the wrong place.

#### Branch + PR Conventions

Branch names: `tracey/tru-<NN>-<short-slug>`. Base branch: `master` (the only
protected branch). PRs default to **draft**; flip to ready only after
`/pr-ready --pr <number>` returns `ACCEPT-READY` and the attestation block is
filled in the PR body (per `github.requirePrReadyBeforeReview: true`).
