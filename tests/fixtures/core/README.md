# TRU-284: Iced baseline before shared-core extraction

Pinned behavior: `282d0f3eba6ebdfe96269d20995b848d336783f5`.
All paths, names, IDs, model names and content added here are synthetic.
`CALLERS.md` pins the production caller inventory to that base.

## Tests

`src/baseline_tests.rs` drives the existing `App::update_event`, selection,
context refresh and source helpers, without running App startup or Iced:

- `workspace_selection_traces`: explicit/bar intents, real Ctrl+2 mapping,
  Usage same/invalid/different selection, task ownership cleanup, ordered
  continuations, plans **and** docs sync, no immediate status request.
- `swipe_park_reorder_and_closed_id_traces`: swipe continuation, keep-running
  park, settle/wake, machine-group reorder retaining UID, removed UID.
- `stopping_park_preserves_identity_and_resume_request_prelude`: stopping
  active/inactive workspaces, saved parked entry, resume request prelude.
- `status_backoff_completion_and_error_traces`: dirty/clean/non-repo interval
  boundaries, fake time, success/error state, both overlapping completion
  orders, inactive/closed tab completion, unchanged-name hash behavior.
- `status_poll_visibility_and_reset_traces`: active-tab-only maintenance,
  hidden Git's 15-second floor, selected diff, in-flight guard, input/context reset.
- `input_wake_and_enter_git_reset_traces`: input at slow/already-fast cadence,
  strict >300-second sleep threshold, wake backdating, Git-entry/re-entry.
- `status_file_order_and_hash_traces`: ordered staged/unstaged/untracked
  lists and list-order sensitivity of the existing hash.
- `source_root_precedence_and_adoption_traces`: task > picked > followed >
  tab/home; plain/task-session adoption and chat/agent/harness/task-host/
  picked/followed launch-root guards.
- `removed_followed_tree_and_source_failure_traces`: active fallback request
  and file jump vs inactive hash reset, exact source errors, remote opaque
  root/capability/dispatch and remote-error retention.
- `persistence_load_action_save_fixtures`: real `WorkspacesFile` disk load,
  normalize, identical active-entry swap/index translation, save, parsed-JSON
  comparison, second normalized load/save equality.
- `persistence_running_park_uses_plain_parked_format`: real running-park
  serializer helper, persisted index and disk load/save.

`source::baseline_tests::opaque_remote_status_error_baseline` checks exact
source error formatting and opaque remote roots without making a connection.

`agentd::git::baseline_tests::cli_parser_and_missing_git_fallback_baseline`
uses temporary repos for nested cwd, detached HEAD, spaces/quotes/tabs/newlines,
and staged rename. A child **test process**, with an empty private PATH, also
exercises the real missing-executable fallback. The parent test environment is
unchanged; no fake Git executable or global PATH race is involved. Quote,
tab and newline filesystem cases run on platforms that support those names;
Windows retains the spaces/rename/nested/detached/fallback cases.

The headless App tests run in child test processes with a private absolute
`GITTERM_V6_CONFIG_DIR`. No application launch, OS input or jobs are executed.
Task commands are input fixtures; dispatch records ordered continuation calls
and local/remote status request targets. Synthetic completions are delivered
explicitly in the chosen order; state and exact error text are asserted after
each transition. `Task::batch` construction order is **not** a claim about
asynchronous completion order or widget-operation execution.

## Persisted fixtures

- `persistence/old.workspaces.json` → `old.saved.json`: omitted pre-repo-model
  defaults, distinct same-folder owned-chat workspaces, active tab/index,
  task/session/chat IDs, legacy Gemini backend, old closed entry, parked/done,
  workspace terminals and per-workspace collapsed Issues groups.
- `persistence/new.workspaces.json` → `new.saved.json`: same sequence with
  explicit shared repos, env/overrides, manual include/exclude and decisions
  repo, repo-held terminal compatibility migration and collapsed repo group.
- `persistence/running-park.saved.json`: new input after `running_parked_config`
  and `persisted_active_index`; restart reads an ordinary stopped parked entry.

Goldens were obtained by running today's loader/helpers/serializer and then
reviewed. Tests have no golden-update mode. JSON is compared structurally;
HashMap byte order is not a contract. Runtime workspace UID/tab ID/poll fields
are not serialized; the durable tab `session_uid`, task/session and chat IDs are.

## Characterized oddities (not fixed)

Each has a `// characterized:` comment next to its assertion:

- Swipe retains Usage and selected task while explicit selection closes Usage
  and clears a task that does not belong to the destination workspace.
- Forced requests overlap; every result clears loading, and arrival order wins,
  including an older request finishing after a newer one.
- Repo name is absent from the status hash, so a rename alone keeps backoff.
- Resume sends status without setting loading or advancing last-poll time.
- Keep-running park has no persisted running flag; restart reads stopped park.
- CLI detached HEAD keeps `main`; git2 fallback reports `HEAD`.
- CLI leaves C-quoted filename text escaped and truncates a spaced rename at
  its last space; fallback returns actual names and separate added/deleted rows.
- Git2 fallback discovers a nested repo but keeps the requested nested root/name.

## Test seams

- `#[cfg(test)]` module `src/baseline_tests.rs`: constructor from production
  initializer values, excluding discovery, service registration and processes;
  thread-local fake clock/backend plus observations (inactive for other tests).
- `status_now!`: runtime expansion remains `Instant::now()`; only status
  maintenance/context/completion/Git-entry and wake timing use the fake clock.
- `#[cfg(test)]` hooks at local/remote request boundaries and synchronous
  continuation/keyboard mapping sites. Fake requests record dispatch and return
  an empty Task; they do not replace selection/status/source transition logic.
- `#[cfg(test)]` swipe input projection: `workspace_slide_scrolled` passes
  viewport.x as a scalar into the real swipe arm, avoiding Iced's private
  Viewport fields. Runtime builds retain the original Viewport event and
  constructor callback via an alias.

No production behavior fixes, new dependencies, cache, core extraction or GPUI
changes are included. Visual execution of continuations and asynchronous job
execution are outside these traces; CLI collection itself runs against fixtures.
