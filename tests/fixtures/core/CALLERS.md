# TRU-284 baseline caller gate

Pinned base: `282d0f3eba6ebdfe96269d20995b848d336783f5` (`origin/v6` at task creation). References below are **base** line numbers, before this PR’s test seams. Do not update them to shifted working-tree lines. No `gpui/` files are part of this gate.

PRs 3–5 must account for every listed site. The owner remains the existing App/workspace/tab/source domain. This inventory includes writes, wrappers, request producers and source/root consumers, not only the maintenance tick. Tests and doc comments are excluded. `fetch_status` is dead code and must stay dead or be removed explicitly. Files/Work summary generations are separate freshness policies; status has none.

## Active selection writes and identity/index bridges

| Base site | Enclosing function / event arm | Code |
| --- | --- | --- |
| `src/main.rs:2484` | `persisted_active_index` | `fn persisted_active_index(active: usize, parked: &[bool]) -> usize {` |
| `src/main.rs:8684` | `usage_view_step` | `fn usage_view_step(` |
| `src/main.rs:10765` | `save_workspace_configs` | `active_workspace: persisted_active_index(` |
| `src/main.rs:13542` | `enter_task_context` | `self.active_workspace_idx = workspace_idx;` |
| `src/main.rs:14182` | `try_launch_task_child` | `self.active_workspace_idx = original_workspace_idx;` |
| `src/main.rs:14292` | `visible_workspace_at` | `fn visible_workspace_at(&self, num: usize) -> Option<usize> {` |
| `src/main.rs:14303` | `slide_order` | `.and_then(\|uid\| self.workspace_index_by_uid(uid));` |
| `src/main.rs:14325` | `begin_slide_to` | `fn begin_slide_to(&mut self, idx: usize) {` |
| `src/main.rs:14334` | `begin_slide_to` | `self.active_workspace_idx = idx;` |
| `src/main.rs:14349` | `normalize_workspace_order` | `fn normalize_workspace_order(&mut self) {` |
| `src/main.rs:14384` | `normalize_workspace_order` | `self.active_workspace_idx = new_active;` |
| `src/main.rs:14486` | `reconcile_managed_task_workspaces` | `self.active_workspace_idx = owner_idx;` |
| `src/main.rs:14494` | `reconcile_managed_task_workspaces` | `self.active_workspace_idx = active_repository_dir` |
| `src/main.rs:14782` | `park_workspace` | `let Some(idx) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:14843` | `park_stopping` | `let kept_active = active_uid.and_then(\|active\| self.workspace_index_by_uid(active));` |
| `src/main.rs:14845` | `park_stopping` | `Some(active) => self.active_workspace_idx = active,` |
| `src/main.rs:14848` | `park_stopping` | `self.active_workspace_idx = self.workspaces.len().saturating_sub(1);` |
| `src/main.rs:14869` | `park_keep_running` | `Some(active) => self.active_workspace_idx = active,` |
| `src/main.rs:14934` | `wake_running_parked` | `let Some(idx) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:14976` | `resume_parked_workspace` | `self.active_workspace_idx = idx;` |
| `src/main.rs:15063` | `close_workspace` | `let Some(idx) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:15206` | `add_repo_to_workspace` | `let idx = self.workspace_index_by_uid(uid)?;` |
| `src/main.rs:15237` | `remove_repo_from_workspace` | `let Some(idx) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:15274` | `sync_workspace_repos` | `if let Some(idx) = self.workspace_index_by_uid(uid) {` |
| `src/main.rs:15468` | `push_and_activate_workspace` | `self.normalize_workspace_order();` |
| `src/main.rs:15469` | `push_and_activate_workspace` | `self.active_workspace_idx = self` |
| `src/main.rs:15470` | `push_and_activate_workspace` | `.workspace_index_by_uid(uid)` |
| `src/main.rs:15479` | `workspace_index_by_uid` | `fn workspace_index_by_uid(&self, uid: u64) -> Option<usize> {` |
| `src/main.rs:15577` | `ensure_remote_workspace_for_chat` | `self.active_workspace_idx = self.open_workspace(workspace);` |
| `src/main.rs:15578` | `ensure_remote_workspace_for_chat` | `self.normalize_workspace_order();` |
| `src/main.rs:16382` | `make_workspace_from_chat` | `if let Some(idx) = self.workspace_index_by_uid(uid) {` |
| `src/main.rs:16589` | `add_chat_to_workspace` | `let Some(target) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:17948` | `confirm_folder_picker` | `if let Some(idx) = self.workspace_index_by_uid(uid) {` |
| `src/main.rs:18036` | `create_remote_workspace_at` | `self.active_workspace_idx = self.open_workspace(workspace);` |
| `src/main.rs:18037` | `create_remote_workspace_at` | `self.normalize_workspace_order();` |
| `src/main.rs:20841` | `new` | `active_workspace_idx: 0,` |
| `src/main.rs:21138` | `new` | `app.active_workspace_idx = ws_file` |
| `src/main.rs:21147` | `new` | `app.normalize_workspace_order();` |
| `src/main.rs:22521` | `open_remote_workspace` | `self.active_workspace_idx = idx;` |
| `src/main.rs:22607` | `open_remote_workspace` | `self.active_workspace_idx = self.open_workspace(workspace);` |
| `src/main.rs:27563` | `update_event / WorkspaceRepoHide` | `let Some(idx) = self.workspace_index_by_uid(uid) else {` |
| `src/main.rs:27580` | `update_event / WorkspaceRepoJoinedUndo` | `.workspace_index_by_uid(uid)` |
| `src/main.rs:27588` | `update_event / WorkspaceRepoJoinedDismiss` | `if let Some(idx) = self.workspace_index_by_uid(uid) {` |
| `src/main.rs:27923` | `update_event / KeyPressed` | `if let Some(idx) = self.visible_workspace_at(num) {` |
| `src/main.rs:28296` | `update_event / KeyPressed` | `if let Some(idx) = self.visible_workspace_at(num) {` |
| `src/main.rs:30534` | `update_event / UsageViewToggle` | `usage_view_step(` |
| `src/main.rs:30543` | `update_event / UsageViewClose` | `usage_view_step(` |
| `src/main.rs:31008` | `update_event / NewWorkOpenExisting` | `ExistingWorkAction::Switch(uid) => match self.workspace_index_by_uid(uid) {` |
| `src/main.rs:31847` | `update_event / WorkspaceSelect` | `usage_view_step(` |
| `src/main.rs:31862` | `update_event / WorkspaceSelect` | `self.begin_slide_to(idx);` |
| `src/main.rs:32003` | `update_event / SlideScrolled` | `self.active_workspace_idx = nearest;` |
| `src/main.rs:32450` | `update_event / WorkspaceCreated` | `self.active_workspace_idx = self.open_workspace(workspace);` |
| `src/main.rs:32451` | `update_event / WorkspaceCreated` | `self.normalize_workspace_order();` |
| `src/main.rs:32680` | `update_event / AttentionJumpNext` | `self.begin_slide_to(ws_idx);` |

## TRU-289 current status caller map

This section supersedes the historical status locations below on this branch.
Selection and source/root ownership remain unchanged for PRs 4–5. The historical
base inventory is retained so every former write/request can be traced.

`GitStatusState::reduce(StatusEvent, Instant) -> Vec<StatusCommand>` mutates the
sole per-tab state. Its inputs include request/reset/input/wake/visibility,
completion/discard, and existing UI metadata/error writes. Commands collect a
resolved root or expose the remaining poll delay. Iced retains source identity,
capability/root guards and job execution. No job generation/freshness guard is added.

| Former owner / caller | Current site | Transition / retained boundary |
| --- | --- | --- |
| TabState fields / constructor | `src/main.rs:5244`; `src/main.rs:5338` | One composed status owner; no writable legacy status fields. |
| Core state / reducer API | `crates/gitterm-core/src/status.rs:51`; `crates/gitterm-core/src/status.rs:131` | Single mutable owner; explicit clock; pure transition to commands. |
| adopt_git_root | `src/main.rs:5395` | Root guards remain here; accepted adoption dispatches Reset. |
| hash / next interval | `crates/gitterm-core/src/status.rs:226`; `crates/gitterm-core/src/status.rs:242` | Hash and cadence now pure core rules; same field/list ordering and thresholds. |
| request_git_status / active-source wrapper | `src/main.rs:10908`; `src/main.rs:10920` | Dispatch Request with explicit prelude; resolve source and actual local request root in adapter. |
| command interpreter / executors | `src/main.rs:10951`; `src/main.rs:10972`; `src/main.rs:10995` | CollectStatus becomes existing Iced Task. SchedulePoll uses existing Tick checks; no second timer. |
| after_resume | `src/main.rs:14916` | Request(Resume): no loading or clock prelude. |
| resume_chat_as_tab / resume_chat_as_chat_tab / remote chat | `src/main.rs:15524`; `src/main.rs:15616`; `src/main.rs:15706` | Request(TabOpened): loading only, via active-source wrapper. |
| startup | `src/main.rs:21090` | Local Request(Refresh): loading and clock, no cadence/hash reset. |
| launch_session_tab | `src/main.rs:21292` | Request(TabOpened) via active-source wrapper. |
| refresh_panels_for_context | `src/main.rs:22410` | Reset then local Request(Refresh); Files jump stays in adapter. |
| Terminal input / CWD | `src/main.rs:24473`; `src/main.rs:24594` | Input; CWD changes Reset then local Request(Refresh). |
| Tick | `src/main.rs:24710` | Current Git-or-diff visibility and resolved root enter reducer; capability/active-tab gates remain in adapter. |
| RemoteSessionLaunched / FileSelect errors | `src/main.rs:27409`; `src/main.rs:27672` | SetError preserves prior loading and clock behavior. |
| EditFile / FolderSelected | `src/main.rs:27424`; `src/main.rs:27637` | Request(TabOpened) via active-source wrapper. |
| OpenWorktree | `src/main.rs:29546` | Request(Refresh) via active-source wrapper. |
| RemoteAgentHandshakeLoaded | `src/main.rs:29623` | Request(Resume): preserve connection path without a prelude. |
| WindowFocused | `src/main.rs:30348` | Wake after existing strict >300s guard; backdate by 60s, keep hash. |
| GitStatusLoaded | `src/main.rs:31151` | Address tab, preserve domain/root guards; Completed or DiscardCompletion enters reducer. Late results for absent tabs ignored. |
| WorkspaceCreated | `src/main.rs:32328` | Request(TabOpened) via active-source wrapper. |
| set_sidebar_mode | `src/main.rs:32742` | Git entry Request(Refresh); source absence ClearLoading. Re-entry remains a no-op. |
| services DTO conversion | `src/services.rs:25`; `src/services.rs:692` | Local collector DTOs and remote opaque-root/error conversion stay outside core. |

The dead `TabState::fetch_status` alternate writer was removed, without adding
any callers. `status_char` remains test-only for its existing tests. Label writes
in task/chat/remote-tab setup now use `SetLabel`; initial remote browser state
uses `InitializeRemote`. Read sites consume `tab.status` directly.

`src/baseline_tests.rs` retains all pre-extraction expected values and goldens;
only composed field paths and the renamed raw source-executor reference change.
Core reducer unit tests also cover both overlapping completion orders, success /
error orders, dirty/clean/non-repo backoff, identity-excluding order-sensitive
hashes, each request prelude, visibility/loading guards, input, wake, reset and
discard. The collector, source transport and selection inventory below remain
historical references, not extracted implementations.

## Status requests, completions, polling writes and reset sites

| Base site | Enclosing function / event arm | Code |
| --- | --- | --- |
| `src/agentd/client.rs:388` | `git_status` | `.git_status(request)` |
| `src/agentd/git.rs:59` | `collect_repo_status` | `pub fn collect_repo_status(repo_path: &Path) -> RepoStatus {` |
| `src/agentd/git.rs:88` | `collect_repo_status` | `return collect_repo_status_git2(status, repo_path);` |
| `src/agentd/git.rs:365` | `collect_repo_status_git2` | `fn collect_repo_status_git2(mut status: RepoStatus, repo_path: &Path) -> RepoStatus {` |
| `src/agentd/server.rs:241` | `git_status` | `let status = super::git::collect_repo_status(&root);` |
| `src/main.rs:5236` | `update_queued_task_request` | `last_poll: Instant,` |
| `src/main.rs:5337` | `new` | `last_poll: Instant::now() - Duration::from_millis(GIT_POLL_FAST_INTERVAL_MS),` |
| `src/main.rs:5338` | `new` | `git_poll_interval_ms: GIT_POLL_FAST_INTERVAL_MS,` |
| `src/main.rs:5339` | `new` | `git_unchanged_streak: 0,` |
| `src/main.rs:5340` | `new` | `last_git_status_hash: None,` |
| `src/main.rs:5341` | `new` | `git_status_loading: false,` |
| `src/main.rs:5342` | `new` | `git_status_error: None,` |
| `src/main.rs:5396` | `adopt_git_root` | `fn adopt_git_root(&mut self, root: &Path) {` |
| `src/main.rs:5406` | `adopt_git_root` | `self.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;` |
| `src/main.rs:5407` | `adopt_git_root` | `self.git_unchanged_streak = 0;` |
| `src/main.rs:5408` | `adopt_git_root` | `self.last_git_status_hash = None;` |
| `src/main.rs:5840` | `fetch_status` | `fn fetch_status(&mut self) {` |
| `src/main.rs:5898` | `fetch_status` | `self.last_poll = Instant::now();` |
| `src/main.rs:6917` | `git_tab_state_hash` | `fn git_tab_state_hash(tab: &TabState) -> u64 {` |
| `src/main.rs:6927` | `next_git_poll_interval_ms` | `fn next_git_poll_interval_ms(is_git_repo: bool, has_changes: bool, unchanged_streak: u32) -> u64 {` |
| `src/main.rs:6948` | `collect_git_status` | `fn collect_git_status(tab_id: usize, repo_path: PathBuf) -> GitStatusSnapshot {` |
| `src/main.rs:6949` | `collect_git_status` | `services::collect_git_status(tab_id, repo_path)` |
| `src/main.rs:11027` | `request_git_status` | `fn request_git_status(tab_id: usize, repo_path: PathBuf) -> Task<Event> {` |
| `src/main.rs:11028` | `request_git_status` | `Self::request_local_git_status(tab_id, repo_path)` |
| `src/main.rs:11033` | `request_git_status_for_active_source` | `fn request_git_status_for_active_source(` |
| `src/main.rs:11040` | `request_git_status_for_active_source` | `Self::request_source_git_status(source, tab_id, repo_path)` |
| `src/main.rs:11049` | `request_source_git_status` | `fn request_source_git_status(` |
| `src/main.rs:11055` | `request_source_git_status` | `return Self::request_local_git_status(tab_id, root);` |
| `src/main.rs:11059` | `request_source_git_status` | `match source.git_status().await {` |
| `src/main.rs:11100` | `request_local_git_status` | `fn request_local_git_status(tab_id: usize, repo_path: PathBuf) -> Task<Event> {` |
| `src/main.rs:11104` | `request_local_git_status` | `match tokio::task::spawn_blocking(move \|\| collect_git_status(tab_id, repo_path))` |
| `src/main.rs:15011` | `after_resume` | `self.request_git_status_for_active_source(tab_id, repo_path)` |
| `src/main.rs:15685` | `resume_chat_as_tab` | `tab.git_status_loading = true;` |
| `src/main.rs:15692` | `resume_chat_as_tab` | `tasks.push(self.request_git_status_for_active_source(tab_id, repo_path));` |
| `src/main.rs:15772` | `resume_chat_as_chat_tab` | `tab.git_status_loading = true;` |
| `src/main.rs:15780` | `resume_chat_as_chat_tab` | `tasks.push(self.request_git_status_for_active_source(tab_id, repo_path));` |
| `src/main.rs:15810` | `resume_remote_chat_as_chat_tab` | `tab.git_status_loading = true;` |
| `src/main.rs:15818` | `resume_remote_chat_as_chat_tab` | `tasks.push(self.request_git_status_for_active_source(tab_id, repo_path));` |
| `src/main.rs:21178` | `new` | `tab.git_status_loading = true;` |
| `src/main.rs:21179` | `new` | `tab.last_poll = Instant::now();` |
| `src/main.rs:21186` | `new` | `startup_tasks.push(Self::request_git_status(tab_id, repo_path));` |
| `src/main.rs:21405` | `launch_session_tab` | `tab.git_status_loading = true;` |
| `src/main.rs:21415` | `launch_session_tab` | `self.request_git_status_for_active_source(tab_id, repo_path),` |
| `src/main.rs:22492` | `refresh_panels_for_context` | `tab.git_status_loading = true;` |
| `src/main.rs:22493` | `refresh_panels_for_context` | `tab.last_poll = Instant::now();` |
| `src/main.rs:22494` | `refresh_panels_for_context` | `tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;` |
| `src/main.rs:22495` | `refresh_panels_for_context` | `tab.git_unchanged_streak = 0;` |
| `src/main.rs:22496` | `refresh_panels_for_context` | `tab.last_git_status_hash = None;` |
| `src/main.rs:22497` | `refresh_panels_for_context` | `git_task = Self::request_local_git_status(tab.id, root.clone());` |
| `src/main.rs:24548` | `update_event / Terminal` | `tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;` |
| `src/main.rs:24549` | `update_event / Terminal` | `tab.git_unchanged_streak = 0;` |
| `src/main.rs:24671` | `update_event / Terminal` | `tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;` |
| `src/main.rs:24672` | `update_event / Terminal` | `tab.git_unchanged_streak = 0;` |
| `src/main.rs:24673` | `update_event / Terminal` | `tab.last_git_status_hash = None;` |
| `src/main.rs:24674` | `update_event / Terminal` | `tab.last_poll = Instant::now();` |
| `src/main.rs:24675` | `update_event / Terminal` | `tab.git_status_loading = true;` |
| `src/main.rs:24676` | `update_event / Terminal` | `let status_task = Self::request_git_status(` |
| `src/main.rs:24801` | `update_event / Tick` | `tab.last_poll = Instant::now();` |
| `src/main.rs:24802` | `update_event / Tick` | `tab.git_status_loading = true;` |
| `src/main.rs:24803` | `update_event / Tick` | `tasks.push(Self::request_source_git_status(` |
| `src/main.rs:27492` | `update_event / RemoteSessionLaunched` | `tab.git_status_error = Some(message);` |
| `src/main.rs:27528` | `update_event / EditFile` | `tab.git_status_loading = true;` |
| `src/main.rs:27537` | `update_event / EditFile` | `self.request_git_status_for_active_source(tab_id, repo_path),` |
| `src/main.rs:27724` | `update_event / FolderSelected` | `tab.git_status_loading = true;` |
| `src/main.rs:27733` | `update_event / FolderSelected` | `self.request_git_status_for_active_source(tab_id, repo_path),` |
| `src/main.rs:27747` | `update_event / FileSelect` | `tab.git_status_error =` |
| `src/main.rs:29659` | `update_event / OpenWorktree` | `tab.git_status_loading = true;` |
| `src/main.rs:29660` | `update_event / OpenWorktree` | `tab.last_poll = Instant::now();` |
| `src/main.rs:29667` | `update_event / OpenWorktree` | `tasks.push(self.request_git_status_for_active_source(tab_id, repo_path));` |
| `src/main.rs:29717` | `update_event / RemoteAgentHandshakeLoaded` | `self.request_git_status_for_active_source(tab_id, repo_path)` |
| `src/main.rs:30409` | `update_event / WindowFocused` | `tab.git_poll_interval_ms = GIT_POLL_FAST_INTERVAL_MS;` |
| `src/main.rs:30410` | `update_event / WindowFocused` | `tab.git_unchanged_streak = 0;` |
| `src/main.rs:30411` | `update_event / WindowFocused` | `tab.last_poll = Instant::now() - Duration::from_secs(60);` |
| `src/main.rs:31214` | `update_event / GitStatusLoaded` | `Event::GitStatusLoaded(snapshot) => {` |
| `src/main.rs:31241` | `update_event / GitStatusLoaded` | `tab.git_status_loading = false;` |
| `src/main.rs:31249` | `update_event / GitStatusLoaded` | `tab.git_status_error = None;` |
| `src/main.rs:31254` | `update_event / GitStatusLoaded` | `tab.last_git_status_hash = None;` |
| `src/main.rs:31257` | `update_event / GitStatusLoaded` | `tab.git_status_error = snapshot.error.clone();` |
| `src/main.rs:31276` | `update_event / GitStatusLoaded` | `tab.adopt_git_root(&snapshot.repo_path);` |
| `src/main.rs:31288` | `update_event / GitStatusLoaded` | `let effective_hash = git_tab_state_hash(tab);` |
| `src/main.rs:31291` | `update_event / GitStatusLoaded` | `tab.git_unchanged_streak = tab.git_unchanged_streak.saturating_add(1);` |
| `src/main.rs:31293` | `update_event / GitStatusLoaded` | `tab.git_unchanged_streak = 0;` |
| `src/main.rs:31299` | `update_event / GitStatusLoaded` | `tab.git_poll_interval_ms = next_git_poll_interval_ms(` |
| `src/main.rs:31304` | `update_event / GitStatusLoaded` | `tab.last_git_status_hash = Some(effective_hash);` |
| `src/main.rs:31315` | `update_event / GitStatusLoaded` | `tab.last_poll = Instant::now();` |
| `src/main.rs:32461` | `update_event / WorkspaceCreated` | `tab.git_status_loading = true;` |
| `src/main.rs:32468` | `update_event / WorkspaceCreated` | `return self.request_git_status_for_active_source(tab_id, repo_path);` |
| `src/main.rs:32865` | `set_sidebar_mode` | `tab.last_poll = Instant::now();` |
| `src/main.rs:32866` | `set_sidebar_mode` | `tab.git_status_loading = true;` |
| `src/main.rs:32876` | `set_sidebar_mode` | `tab.git_status_loading = false;` |
| `src/main.rs:32881` | `set_sidebar_mode` | `Self::request_source_git_status(source, tab_id, repo_path.clone());` |
| `src/services.rs:25` | `collect_git_status` | `pub(crate) fn collect_git_status(tab_id: usize, repo_path: PathBuf) -> GitStatusSnapshot {` |
| `src/services.rs:28` | `collect_git_status` | `let status = gitterm::agentd::git::collect_repo_status(&repo_path);` |
| `src/services.rs:107` | `collect_git_worktrees` | `let status = collect_git_status(tab_id, entry.path.clone());` |
| `src/source.rs:298` | `git_status` | `gitterm::agentd::git::collect_repo_status(&root)` |
| `src/source.rs:318` | `git_status` | `.git_status(workspace_id.clone(), root.clone())` |

## Source/root resolution definitions and callers

| Base site | Enclosing function / event arm | Code |
| --- | --- | --- |
| `src/agentd/client.rs:371` | `git_status` | `pub async fn git_status(` |
| `src/agentd/git.rs:102` | `collect_repo_status` | `.args(["--no-optional-locks", "rev-parse", "--show-toplevel"])` |
| `src/agentd/git.rs:223` | `collect_file_diff` | `let Ok(repo) = Repository::open(repo_root).or_else(\|_\| Repository::discover(repo_root)) else {` |
| `src/agentd/git.rs:366` | `collect_repo_status_git2` | `let Ok(repo) = Repository::open(repo_path).or_else(\|_\| Repository::discover(repo_path)) else {` |
| `src/agentd/server.rs:230` | `git_status` | `async fn git_status(` |
| `src/main.rs:5323` | `new` | `let is_git_repo = Repository::discover(&repo_path).is_ok();` |
| `src/main.rs:5391` | `set_local_dir` | `self.current_dir = dir;` |
| `src/main.rs:5405` | `adopt_git_root` | `self.repo_path = root.to_path_buf();` |
| `src/main.rs:5841` | `fetch_status` | `if let Ok(repo) = Repository::open(&self.repo_path) {` |
| `src/main.rs:6078` | `fetch_diff` | `let Ok(repo) = Repository::open(&self.repo_path) else {` |
| `src/main.rs:6960` | `paths_equal` | `fn paths_equal(a: &Path, b: &Path) -> bool {` |
| `src/main.rs:9137` | `workspace_location_from_config` | `fn workspace_location_from_config(` |
| `src/main.rs:10503` | `scroll_to_active_tab` | `let task_context_id = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:10640` | `workspace_config` | `.remote_session_for_workspace(ws)` |
| `src/main.rs:10715` | `save_workspace_configs` | `if let Some(session) = self.remote_session_for_workspace(ws) {` |
| `src/main.rs:11038` | `request_git_status_for_active_source` | `match self.source_for_active_tab() {` |
| `src/main.rs:11474` | `find_chat_tab` | `.filter(\|(_, ws)\| self.remote_agent_id_for_workspace(ws) == chat.remote_id.as_deref())` |
| `src/main.rs:11490` | `tab_remote_id` | `.and_then(\|ws\| self.remote_agent_id_for_workspace(ws))` |
| `src/main.rs:11620` | `attach_running_remote_chat_tabs` | `.filter(\|ws\| self.remote_agent_id_for_workspace(ws) == Some(remote_id))` |
| `src/main.rs:11859` | `chat_checkout_state` | `if let Some(remote_id) = self.remote_agent_id_for_workspace(ws) {` |
| `src/main.rs:12038` | `chat_guidelines_on` | `tab::AgentBackend::Claude => self.remote_agent_id_for_workspace(ws).is_none(),` |
| `src/main.rs:12095` | `active_task_context_id` | `fn active_task_context_id(&self) -> Option<&str> {` |
| `src/main.rs:12109` | `task_context_worktree_root` | `fn task_context_worktree_root(&self) -> Option<PathBuf> {` |
| `src/main.rs:12110` | `task_context_worktree_root` | `let task_id = self.active_task_context_id()?;` |
| `src/main.rs:12116` | `task_context_worktree_root` | `if !paths_equal(directory, &workspace.dir) {` |
| `src/main.rs:12127` | `followed_worktree_root` | `fn followed_worktree_root(&self) -> Option<PathBuf> {` |
| `src/main.rs:12128` | `followed_worktree_root` | `if self.active_workspace_is_remote() {` |
| `src/main.rs:12133` | `followed_worktree_root` | `.panel_root()` |
| `src/main.rs:12140` | `panel_context_root` | `fn panel_context_root(&self) -> Option<PathBuf> {` |
| `src/main.rs:12141` | `panel_context_root` | `self.task_context_worktree_root()` |
| `src/main.rs:12142` | `panel_context_root` | `.or_else(\|\| self.panel_repo_root())` |
| `src/main.rs:12143` | `panel_context_root` | `.or_else(\|\| self.followed_worktree_root())` |
| `src/main.rs:12148` | `panel_repo_root` | `fn panel_repo_root(&self) -> Option<PathBuf> {` |
| `src/main.rs:12150` | `panel_repo_root` | `(self.active_tab()?.id == *tab_id && !self.active_workspace_is_remote())` |
| `src/main.rs:12218` | `apply_worktree_candidates` | `self.panel_context_root()` |
| `src/main.rs:12231` | `apply_worktree_candidates` | `if changed && is_active_tab && self.panel_context_root() != before {` |
| `src/main.rs:12244` | `worktree_task` | `.is_some_and(\|worktree\| paths_equal(worktree, path))` |
| `src/main.rs:12334` | `canonical_task_workspace_directory` | `if paths_equal(&canonical, directory) {` |
| `src/main.rs:12367` | `task_belongs_to_workspace` | `paths_equal(&directory, root) \|\| paths_equal(&directory, &workspace.dir)` |
| `src/main.rs:12506` | `acknowledge_active_task_attention` | `if let Some(task_id) = self.active_task_context_id().map(str::to_string) {` |
| `src/main.rs:12895` | `task_switcher_entries` | `let current = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:12976` | `task_switcher_default_index` | `let current = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:13450` | `pending_task_from_control` | `let came_from_managed_worktree = !paths_equal(&repository_path, &input.repository_path);` |
| `src/main.rs:14180` | `try_launch_task_child` | `&& paths_equal(&workspace.dir, &original_dir)` |
| `src/main.rs:14431` | `reconcile_managed_task_workspaces` | `if paths_equal(&canonical, &workspace.dir) {` |
| `src/main.rs:14444` | `reconcile_managed_task_workspaces` | `&& paths_equal(&workspace.dir, &repository_dir)` |
| `src/main.rs:14468` | `reconcile_managed_task_workspaces` | `.is_some_and(\|active\| paths_equal(active, &managed.dir));` |
| `src/main.rs:14499` | `reconcile_managed_task_workspaces` | `.position(\|workspace\| paths_equal(&workspace.dir, directory))` |
| `src/main.rs:14648` | `workspace_chat_entries` | `let entries = self.chat_entries_for_machine(self.remote_agent_id_for_workspace(ws));` |
| `src/main.rs:14658` | `park_note_entries` | `let entries = self.chat_entries_for_machine(self.remote_agent_id_for_workspace(ws));` |
| `src/main.rs:14717` | `open_park_sheet` | `let remote = self.remote_agent_id_for_workspace(ws).is_some();` |
| `src/main.rs:16049` | `poll_work_notes` | `let root = self.work_notes.root().to_path_buf();` |
| `src/main.rs:16213` | `local_chat_tab_untitled` | `.filter(\|ws\| self.remote_agent_id_for_workspace(ws).is_none())` |
| `src/main.rs:16238` | `overview_key` | `if self.active_workspace_is_remote() {` |
| `src/main.rs:16918` | `work_documents_request` | `\|\| self.remote_agent_id_for_workspace(ws).is_some()` |
| `src/main.rs:18420` | `delegation_chat_tab_id` | `let remote = self.workspace_has_remote_identity(ws);` |
| `src/main.rs:18467` | `delegation_caller` | `remote: self.workspace_has_remote_identity(workspace),` |
| `src/main.rs:20289` | `restore_workspace_from_config` | `let location = workspace_location_from_config(` |
| `src/main.rs:20424` | `restore_workspace_from_config` | `Repository::discover(&tab_dir)` |
| `src/main.rs:20649` | `restore_workspace_from_config` | `paths_equal(&tab.repo_path, &workspace.dir)` |
| `src/main.rs:20650` | `restore_workspace_from_config` | `\|\| paths_equal(&tab.current_dir, &workspace.dir)` |
| `src/main.rs:21175` | `new` | `if !app.active_workspace_is_remote() {` |
| `src/main.rs:21243` | `add_tab_to_workspace_with_command` | `if let Some(session) = self.remote_session_for_workspace(workspace) {` |
| `src/main.rs:21382` | `launch_session_tab` | `let remote = match self.source_for_active_tab() {` |
| `src/main.rs:21427` | `launch_session_tab` | `.and_then(\|ws\| self.remote_agent_id_for_workspace(ws))` |
| `src/main.rs:21508` | `remote_session_attach_command` | `let remote_id = self.remote_agent_id_for_workspace(workspace)?.to_string();` |
| `src/main.rs:22128` | `active_workspace_mut` | `let remote_id = self.remote_agent_id_for_workspace(workspace)?;` |
| `src/main.rs:22244` | `ensure_active_remote_agent_connection` | `.and_then(\|workspace\| self.remote_agent_id_for_workspace(workspace))` |
| `src/main.rs:22311` | `workspace_has_remote_identity` | `fn workspace_has_remote_identity(&self, workspace: &Workspace) -> bool {` |
| `src/main.rs:22313` | `workspace_has_remote_identity` | `\|\| self.remote_session_for_workspace(workspace).is_some()` |
| `src/main.rs:22316` | `active_workspace_is_remote` | `fn active_workspace_is_remote(&self) -> bool {` |
| `src/main.rs:22318` | `active_workspace_is_remote` | `.is_some_and(\|workspace\| self.workspace_has_remote_identity(workspace))` |
| `src/main.rs:22323` | `tab_belongs_to_remote_workspace` | `self.workspace_has_remote_identity(workspace)` |
| `src/main.rs:22346` | `source_for_active_tab` | `fn source_for_active_tab(&self) -> Result<WorkspaceSource, String> {` |
| `src/main.rs:22358` | `source_for_active_tab` | `let root = self.panel_context_root().unwrap_or_else(\|\| {` |
| `src/main.rs:22392` | `browse_active_tab_to` | `let source = self.source_for_active_tab();` |
| `src/main.rs:22395` | `browse_active_tab_to` | `.followed_worktree_root()` |
| `src/main.rs:22396` | `browse_active_tab_to` | `.or_else(\|\| self.panel_repo_root());` |
| `src/main.rs:22405` | `browse_active_tab_to` | `Ok(source) if !source.owns(&dir) => source.root(),` |
| `src/main.rs:22417` | `browse_active_tab_to` | `tab.current_dir = local.to_path_buf();` |
| `src/main.rs:22435` | `refresh_worktrees_for_active_tab` | `if self.active_workspace_is_remote() {` |
| `src/main.rs:22450` | `refresh_worktrees_for_active_tab` | `.is_some_and(\|path\| paths_equal(path, &workspace_dir))` |
| `src/main.rs:22486` | `refresh_panels_for_context` | `let Ok(WorkspaceSource::Local { root }) = self.source_for_active_tab() else {` |
| `src/main.rs:22518` | `open_remote_workspace` | `self.remote_session_for_workspace(workspace)` |
| `src/main.rs:22528` | `open_remote_workspace` | `.is_some_and(\|workspace\| self.remote_agent_id_for_workspace(workspace).is_some());` |
| `src/main.rs:22620` | `open_remote_workspace` | `.is_some_and(\|workspace\| self.remote_agent_id_for_workspace(workspace).is_some())` |
| `src/main.rs:22632` | `refresh_remote_sessions` | `.and_then(\|workspace\| self.remote_session_for_workspace(workspace))` |
| `src/main.rs:22701` | `open_remote_session_tab_with_command` | `let local_cwd = if self.workspace_has_remote_identity(&self.workspaces[ws_idx]) {` |
| `src/main.rs:22720` | `open_remote_session_tab` | `.and_then(\|workspace\| self.remote_session_for_workspace(workspace))` |
| `src/main.rs:22737` | `open_remote_codex_session_tab` | `.and_then(\|workspace\| self.remote_session_for_workspace(workspace))` |
| `src/main.rs:22757` | `open_remote_claude_session_tab` | `.and_then(\|workspace\| self.remote_session_for_workspace(workspace))` |
| `src/main.rs:23013` | `poll_overview_transcripts` | `if self.remote_agent_id_for_workspace(workspace).is_some() {` |
| `src/main.rs:23188` | `chat_overview_key` | `self.remote_agent_id_for_workspace(workspace)` |
| `src/main.rs:23275` | `build_chat_overview` | `let remote_id = self.remote_agent_id_for_workspace(workspace);` |
| `src/main.rs:24654` | `update_event / Terminal` | `tab.current_dir = dir.clone();` |
| `src/main.rs:24670` | `update_event / Terminal` | `tab.repo_path = dir.clone();` |
| `src/main.rs:24777` | `update_event / Tick` | `.source_for_active_tab()` |
| `src/main.rs:24819` | `update_event / Tick` | `.is_some_and(\|path\| paths_equal(path, workspace_dir))` |
| `src/main.rs:24839` | `update_event / Tick` | `if !self.active_workspace_is_remote() && !self.rail_worktrees_loading {` |
| `src/main.rs:24851` | `update_event / Tick` | `.is_some_and(\|path\| paths_equal(path, &workspace_dir));` |
| `src/main.rs:25004` | `update_event / TabSelect` | `let context_root_before = self.panel_context_root();` |
| `src/main.rs:25028` | `update_event / TabSelect` | `let panels_task = if self.panel_context_root() != context_root_before {` |
| `src/main.rs:26064` | `update_event / AgentSetCheckout` | `.find(\|c\| paths_equal(&c.path, &path))` |
| `src/main.rs:26086` | `update_event / AgentSetCheckout` | `tab.repo_path = choice.path.clone();` |
| `src/main.rs:26526` | `update_event / AgentOpenCheckout` | `&& self.remote_agent_id_for_workspace(ws).is_none();` |
| `src/main.rs:27344` | `update_event / ToggleWorktreeFollow` | `let before = self.panel_context_root();` |
| `src/main.rs:27348` | `update_event / ToggleWorktreeFollow` | `if changed && self.panel_context_root() != before {` |
| `src/main.rs:27741` | `update_event / FileSelect` | `.source_for_active_tab()` |
| `src/main.rs:27783` | `update_event / FileSelect` | `if let Ok(source) = self.source_for_active_tab() {` |
| `src/main.rs:27797` | `update_event / FileSelectByIndex` | `.source_for_active_tab()` |
| `src/main.rs:27842` | `update_event / FileSelectByIndex` | `if let Ok(source) = self.source_for_active_tab() {` |
| `src/main.rs:28281` | `update_event / KeyPressed` | `return Task::done(self.active_task_context_id().map_or(` |
| `src/main.rs:28305` | `update_event / KeyPressed` | `&& self.active_task_context_id().is_some()` |
| `src/main.rs:28317` | `update_event / KeyPressed` | `} else if c == "0" && self.active_task_context_id().is_some() {` |
| `src/main.rs:28324` | `update_event / KeyPressed` | `self.active_task_context_id().map_or_else(` |
| `src/main.rs:28346` | `update_event / KeyPressed` | `&& self.active_task_context_id().is_some()` |
| `src/main.rs:28624` | `update_event / TaskMetadataResolved` | `let current_context = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:28759` | `update_event / TaskOverview` | `let Some(task_id) = self.active_task_context_id().map(str::to_string) else {` |
| `src/main.rs:29014` | `update_event / TaskArchive` | `let was_focused = self.active_task_context_id() == Some(task_id.as_str());` |
| `src/main.rs:29042` | `update_event / TaskArchiveAllFailed` | `let focused = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:29082` | `update_event / TaskArchiveFinished` | `let focused = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:29431` | `update_event / WorktreeRemove` | `if paths_equal(&path, &workspace_dir) {` |
| `src/main.rs:29444` | `update_event / WorktreeRemove` | `.find(\|entry\| paths_equal(&entry.path, &path))` |
| `src/main.rs:29479` | `update_event / WorktreeRemoveCompleted` | `.retain(\|entry\| !paths_equal(&entry.path, &path));` |
| `src/main.rs:29483` | `update_event / WorktreeRemoveCompleted` | `.retain(\|entry\| !paths_equal(&entry.path, &path));` |
| `src/main.rs:29542` | `update_event / OpenRepoPanel` | `return match self.panel_repo_root() {` |
| `src/main.rs:29590` | `update_event / SelectGitWorktree` | `.is_some_and(\|source\| paths_equal(source, dir))` |
| `src/main.rs:29596` | `update_event / SelectGitWorktree` | `.any(\|worktree\| paths_equal(&worktree.path, &path))` |
| `src/main.rs:29608` | `update_event / SelectGitBranch` | `.is_some_and(\|source\| paths_equal(source, dir))` |
| `src/main.rs:29626` | `update_event / OpenWorktree` | `.is_some_and(\|source\| paths_equal(source, &workspace.dir));` |
| `src/main.rs:29631` | `update_event / OpenWorktree` | `&& paths_equal(&worktree.path, &path)` |
| `src/main.rs:29641` | `update_event / OpenWorktree` | `paths_equal(&tab.repo_path, &path) \|\| paths_equal(&tab.current_dir, &path)` |
| `src/main.rs:29710` | `update_event / RemoteAgentHandshakeLoaded` | `.and_then(\|ws\| self.remote_agent_id_for_workspace(ws))` |
| `src/main.rs:29921` | `update_event / ViewFile` | `SourcePath::Remote(_) => match self.source_for_active_tab() {` |
| `src/main.rs:30106` | `update_event / ToggleTheme` | `if let Ok(source) = self.source_for_active_tab() {` |
| `src/main.rs:31226` | `update_event / GitStatusLoaded` | `.is_some_and(\|path\| paths_equal(path, &snapshot.repo_path))` |
| `src/main.rs:31232` | `update_event / GitStatusLoaded` | `.panel_repo_root()` |
| `src/main.rs:31233` | `update_event / GitStatusLoaded` | `.is_some_and(\|dir\| paths_equal(&dir, &snapshot.repo_path));` |
| `src/main.rs:31234` | `update_event / GitStatusLoaded` | `let context_root_before = self.panel_context_root();` |
| `src/main.rs:31247` | `update_event / GitStatusLoaded` | `let was_following = tab.worktree_follow.panel_root().is_some();` |
| `src/main.rs:31248` | `update_event / GitStatusLoaded` | `if tab.worktree_follow.check_removed(Path::is_dir) && was_following {` |
| `src/main.rs:31273` | `update_event / GitStatusLoaded` | `.followed_paths()` |
| `src/main.rs:31274` | `update_event / GitStatusLoaded` | `.any(\|path\| paths_equal(path, &snapshot.repo_path))` |
| `src/main.rs:31325` | `update_event / GitWorktreesLoaded` | `&& !self.active_workspace_is_remote()` |
| `src/main.rs:31328` | `update_event / GitWorktreesLoaded` | `.is_some_and(\|workspace\| paths_equal(&workspace.dir, &snapshot.repo_path))` |
| `src/main.rs:31419` | `update_event / GitWorktreesLoaded` | `.any(\|worktree\| paths_equal(&worktree.path, path)),` |
| `src/main.rs:32690` | `update_event / AttentionJumpNext` | `let current_task = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:32821` | `set_sidebar_mode` | `let active_source = self.source_for_active_tab().ok();` |
| `src/main.rs:32826` | `set_sidebar_mode` | `let legacy_remote = self.active_workspace_is_remote()` |
| `src/main.rs:32829` | `set_sidebar_mode` | `.is_none_or(\|workspace\| self.remote_agent_id_for_workspace(workspace).is_none());` |
| `src/main.rs:34842` | `view_task_switcher` | `let current = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:35027` | `view_tab_picker` | `let task_context_id = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:38247` | `view_session_bar` | `let task_context_id = self.active_task_context_id().map(str::to_string);` |
| `src/main.rs:38257` | `view_session_bar` | `.and_then(\|ws\| self.remote_agent_id_for_workspace(ws));` |
| `src/main.rs:38273` | `view_session_bar` | `self.workspace_has_remote_identity(ws),` |
| `src/main.rs:38728` | `view_session_bar` | `.filter(\|_\| !self.active_workspace_is_remote())` |
| `src/main.rs:38850` | `view_session_bar` | `} else if tab.task_id.is_none() && self.active_task_context_id().is_some() {` |
| `src/main.rs:38903` | `view_session_bar` | `if self.workspace_has_remote_identity(ws) {` |
| `src/main.rs:38941` | `view_session_bar` | `if self.remote_agent_id_for_workspace(ws).is_some() {` |
| `src/main.rs:38945` | `view_session_bar` | `let Some(session) = self.remote_session_for_workspace(ws) else {` |
| `src/main.rs:39087` | `view_session_bar` | `let session = self.remote_session_for_workspace(ws)?;` |
| `src/main.rs:39131` | `view_session_bar` | `let Some(remote_id) = self.remote_agent_id_for_workspace(ws) else {` |
| `src/main.rs:39502` | `files_roots_for_active` | `let workspace_is_remote = self.active_workspace_is_remote();` |
| `src/main.rs:39507` | `files_roots_for_active` | `.source_for_active_tab()` |
| `src/main.rs:39530` | `files_roots_for_active` | `SidebarMode::Files if files_roots.is_some() && self.panel_repo_root().is_some() => {` |
| `src/main.rs:39552` | `files_roots_for_active` | `Some(roots) if self.panel_repo_root().is_none() => self.view_files_roots(&roots),` |
| `src/main.rs:39603` | `files_roots_for_active` | `.filter(\|workspace\| self.remote_agent_id_for_workspace(workspace).is_some())` |
| `src/main.rs:40152` | `files_roots_for_active` | `if self.active_workspace_is_remote() {` |
| `src/main.rs:40803` | `view_task_rail` | `let general_selected = self.active_task_context_id().is_none();` |
| `src/main.rs:41069` | `view_task_rail` | `let selected = self.active_task_context_id() == Some(task.task_id.as_str());` |
| `src/main.rs:41235` | `view_task_rail` | `.is_some_and(\|path\| paths_equal(path, &workspace.dir))` |
| `src/main.rs:41348` | `view_task_detail` | `let selected = self.active_task_context_id().and_then(\|task_id\| {` |
| `src/main.rs:42602` | `view_task_publish_modal` | `.source_for_active_tab()` |
| `src/main.rs:43369` | `view_chats_open_section` | `let machine = self.remote_agent_id_for_workspace(workspace);` |
| `src/main.rs:43640` | `view_chats_open_section` | `let machine = self.remote_agent_id_for_workspace(ws);` |
| `src/main.rs:46825` | `git_source_matches` | `.is_some_and(\|path\| paths_equal(path, &workspace.dir))` |
| `src/main.rs:47071` | `git_source_matches` | `Some(GitSelection::Worktree(path)) if paths_equal(path, &worktree.path)` |
| `src/main.rs:47121` | `git_source_matches` | `.find(\|worktree\| paths_equal(&worktree.path, path))` |
| `src/main.rs:47341` | `open_worktree_label` | `.any(\|tab\| paths_equal(&tab.repo_path, path) \|\| paths_equal(&tab.current_dir, path))` |
| `src/main.rs:47637` | `worktree_usage_label` | `if paths_equal(&tab.repo_path, path) \|\| paths_equal(&tab.current_dir, path) {` |
| `src/main.rs:47650` | `worktree_usage_label` | `if paths_equal(&terminal.cwd, path) {` |
| `src/main.rs:47722` | `worktree_usage_label` | `.source_for_active_tab()` |
| `src/services.rs:135` | `collect_git_worktrees` | `is_current: paths_equal(&path, &repo_path),` |
| `src/services.rs:312` | `paths_equal` | `fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {` |
| `src/source.rs:228` | `root` | `pub fn root(&self) -> SourcePath {` |
| `src/source.rs:250` | `list_dir` | `display_dir: format!("{}/", self.root().display()),` |
| `src/source.rs:251` | `list_dir` | `current_dir: self.root(),` |
| `src/source.rs:279` | `list_dir` | `display_dir: format!("{}/", source.root().display()),` |
| `src/source.rs:280` | `list_dir` | `current_dir: source.root(),` |
| `src/source.rs:293` | `git_status` | `pub async fn git_status(&self) -> Result<SourceGitStatus, String> {` |
| `src/source.rs:574` | `remote_parent_within_root` | `fn remote_parent_within_root(current_dir: &str, root: &str) -> Option<String> {` |
| `src/worktree_follow.rs:120` | `resolve_for_match` | `pub fn resolve_for_match(path: &Path) -> PathBuf {` |
| `src/worktree_follow.rs:287` | `decide` | `pub fn decide(candidate: &Path, own_root: &Path, worktrees: &[WorktreeInfo]) -> FollowDecision {` |
| `src/worktree_follow.rs:302` | `decide_all` | `pub fn decide_all(` |
| `src/worktree_follow.rs:470` | `panel_root` | `pub fn panel_root(&self) -> Option<&Path> {` |
| `src/worktree_follow.rs:480` | `apply` | `let before = self.panel_root().map(Path::to_path_buf);` |
| `src/worktree_follow.rs:496` | `apply` | `before.as_deref() != self.panel_root()` |
| `src/worktree_follow.rs:508` | `check_removed` | `pub fn check_removed(&mut self, exists: impl Fn(&Path) -> bool) -> bool {` |
| `src/worktree_follow.rs:529` | `followed_paths` | `pub fn followed_paths(&self) -> impl Iterator<Item = &Path> {` |

## Persistence bridge and merged prerequisite anchors

Active selection is a runtime index; workspace `uid`, tab `id`, poll state and status jobs are not persisted. `session_uid`, `chat_session_id`, `task_id`, and `task_session_id` are persisted. The fixture action reorders the two active entries and translates the saved active index explicitly; parked/done entries remain in file order.

| Base anchor | Boundary |
| --- | --- |
| `src/main.rs:10599`, `:10630`, `:10707` | Sole App save/config writer and active-index bridge |
| `src/main.rs:20259`, `:21138` | Restore tabs/session launch roots and persisted active index |
| `src/config.rs:2091`, `:2273`, `:2470`, `:2596`, `:2602`, `:2612` | DTO/default/normalize/load/save implementation |
| `src/main.rs:2475`, `:14956` | Running park persistence bridge and settle-after-selection |
| `src/source.rs:195`, `:228`, `:293` | Capabilities, opaque roots and local/remote status adapter |

The design paper’s earlier line numbers are superseded by this inventory. All prerequisite branches are already in the pinned base: stale/exhausted launch gates and idle estimates live in `src/plan_limits.rs` (TRU-232/272); `collapsed_issue_groups` defaults/restore/save in `src/config.rs` and `src/main.rs` (TRU-243); launch guidelines in `src/guidelines.rs` and main launch/restore callers (TRU-259/265); missing-worktree archive selector at `src/main.rs:6807` (TRU-269); Copy Markdown IPC/update/WebView in main/webview (TRU-270); closed PR row labels in `src/work_prs.rs` (TRU-271); legacy Gemini configs and read-only restore guards in `src/tab/agent.rs` and config (TRU-273). These behaviors are not extracted or changed here.

Audit searches at the pinned base: assignments/initializers of
`active_workspace_idx`; calls/definitions of all `request_*git_status`,
`collect_*status`, `fetch_status`, `next_git_poll_interval_ms`,
`git_tab_state_hash`, `adopt_git_root`; writes of `last_poll`, interval,
streak, hash, loading and error; all `source_for_active_tab`, task/picked/
followed/context root helpers, `WorkspaceSource::root`, remote status
client/server entry points, repository discovery and root/current-dir writes.
Source error/capability guards and the dead `fetch_status` path are included.
The App selection arm's non-status continuations remain the Iced adapter's
responsibility; a migration must retain both plans and docs synchronization.
