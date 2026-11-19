# TRU-274 C0 caller inventory

Pinned base: `f6df7cae36ac8af2ef758b5619a86ef5f56a6972` (`origin/v6` at task creation). All line numbers below refer to that commit, before test seams. `src/main.rs` is read-only for this task.

Before extracting C1 or a later harness leaf, account for both the owner/update sites and transport/display consumers. This is a migration checklist, not authorization to move or change any of them.

| Base site | Definition / caller |
| --- | --- |
| `src/harness/mod.rs:48` | `pub enum RuntimeRequestKind {` |
| `src/harness/mod.rs:100` | `pub enum HarnessEvent {` |
| `src/harness/mod.rs:256` | `pub enum HarnessSession {` |
| `src/harness/mod.rs:288` | `pub enum RuntimeDecision {` |
| `src/harness/mod.rs:346` | `pub fn validate_images(images: &[ImageAttachment]) -> Result<(), String> {` |
| `src/harness/mod.rs:407` | `pub enum HarnessCommand {` |
| `src/harness/claude.rs:182` | `pub fn parse_line(&mut self, line: &str) -> Vec<ParsedFrame> {` |
| `src/harness/claude.rs:862` | `pub fn permission_result(` |
| `src/harness/claude.rs:1140` | `pub fn spawn(config: ClaudeSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {` |
| `src/harness/claude.rs:1200` | `impl Drop for ClaudeSession {` |
| `src/harness/claude.rs:1305` | `pub fn spawn_remote(` |
| `src/harness/claude.rs:1318` | `pub fn attach_remote(` |
| `src/harness/claude.rs:1595` | `async fn wait_to_retry(&mut self) -> bool {` |
| `src/harness/claude.rs:1963` | `async fn write(&mut self, line: String) -> Result<(), String> {` |
| `src/harness/claude.rs:2050` | `async fn run_session_program(` |
| `src/harness/acp.rs:358` | `pub struct SessionSettings {` |
| `src/harness/acp.rs:895` | `pub fn map(&mut self, update: &Value) -> Vec<Mapped> {` |
| `src/harness/acp.rs:966` | `pub fn permission(` |
| `src/harness/acp.rs:1017` | `pub fn permission_outcome(decision: &RuntimeDecision, options: &[Value]) -> Result<Value, String> {` |
| `src/harness/acp.rs:1136` | `pub struct HistoryBuilder {` |
| `src/harness/acp.rs:1205` | `pub struct PromptQueue {` |
| `src/harness/acp.rs:1271` | `pub fn prompt_blocks(prompt: &UserPrompt, images_supported: bool) -> Vec<Value> {` |
| `src/harness/acp.rs:1388` | `impl Drop for AcpSession {` |
| `src/harness/acp.rs:1729` | `async fn finish_history(&mut self) -> Result<(), String> {` |
| `src/harness/acp.rs:1986` | `async fn on_line(&mut self, line: &str) -> Result<(), String> {` |
| `src/harness/acp.rs:2028` | `async fn on_command(&mut self, cmd: HarnessCommand) -> Result<(), String> {` |
| `src/harness/acp.rs:2144` | `async fn run_session(` |
| `src/tab/agent.rs:206` | `pub(crate) fn submitted_prompt(prompt: &UserPrompt, queued: bool) -> Self {` |
| `src/tab/agent.rs:261` | `pub(crate) fn webview_payload(&self) -> Option<serde_json::Value> {` |
| `src/tab/agent.rs:366` | `pub(crate) fn history_to_load(&self) -> Option<&str> {` |
| `src/tab/agent.rs:377` | `pub(crate) fn shows_prompt(&self, id: &str) -> bool {` |
| `src/tab/agent.rs:388` | `pub(crate) fn record(&mut self, ev: AgentEvent) -> (Vec<AgentEvent>, Vec<AgentEvent>) {` |
| `src/tab/agent.rs:467` | `pub(crate) fn claude_session_config(` |
| `src/tab/agent.rs:503` | `pub(crate) fn acp_session_config(` |
| `src/tab/agent.rs:550` | `pub(crate) fn submit_queues(&self) -> bool {` |
| `src/tab/agent.rs:576` | `pub(crate) fn submit_queues(` |
| `src/tab/agent.rs:747` | `pub(crate) fn request_stop(&self) {` |
| `src/tab/agent.rs:759` | `pub(crate) fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {` |
| `src/tab/agent.rs:776` | `pub(crate) fn spawn_agent_task(` |
| `src/tab/agent.rs:824` | `async fn agent_loop(` |
| `src/tab/agent.rs:873` | `async fn run_turn(` |
| `src/tab/agent.rs:955` | `fn build_command(` |
| `src/main.rs:606` | `fn history_timeline(` |
| `src/main.rs:11577` | `fn stop_remote_chat_for_closing_tab(&mut self, tab_id: usize) {` |
| `src/main.rs:11608` | `fn remote_chat_link_changed(&mut self, tab_id: usize, link: HarnessEvent) -> Task<Event> {` |
| `src/main.rs:11668` | `fn agent_prompt_echoed(` |
| `src/main.rs:25023` | `Event::TabClose(idx) => {` |
| `src/main.rs:25183` | `AgentIpcMessage::Withdraw { tab_id, id } => {` |
| `src/main.rs:25218` | `Task::done(Event::AgentSetPermissionMode(tab_id, mode))` |
| `src/main.rs:25221` | `Task::done(Event::AgentSetModel(tab_id, model))` |
| `src/main.rs:25224` | `Task::done(Event::AgentSetEffort(tab_id, effort))` |
| `src/main.rs:25230` | `Task::done(Event::AgentSetCheckout(tab_id, path))` |
| `src/main.rs:25321` | `Event::AgentSubmitPrompt(tab_id, prompt) => (tab_id, prompt, None),` |
| `src/main.rs:25756` | `Event::AgentAnswerRequest(tab_id, request_id, decision) => {` |
| `src/main.rs:25781` | `Event::AgentSetPermissionMode(tab_id, mode) => {` |
| `src/main.rs:25857` | `Event::AgentSetModel(tab_id, model) => {` |
| `src/main.rs:25920` | `Event::AgentSetEffort(tab_id, effort) => {` |
| `src/main.rs:26013` | `Event::AgentSetCheckout(tab_id, path) => {` |
| `src/main.rs:26681` | `Event::AgentStopRequested(tab_id) => {` |
| `src/main.rs:26790` | `Event::AgentHistoryUnavailable(tab_id, error) => {` |
| `src/main.rs:26809` | `Event::AgentHistoryLoaded(tab_id, history) => {` |
| `src/main.rs:26868` | `tab::AgentEvent::Harness(HarnessEvent::HistoryReplayed { entries }),` |
| `src/main.rs:26898` | `Event::AgentEventReceived(tab_id, ev) => {` |
| `src/harness/transcript.rs:38` | `pub enum TranscriptEntry {` |
| `src/harness/transcript.rs:49` | `pub struct TimedEntry {` |
| `src/harness/transcript.rs:55` | `pub fn load_claude_history(path: &Path) -> std::io::Result<Vec<TimedEntry>> {` |
| `src/harness/transcript.rs:62` | `pub fn parse_claude_transcript(text: &str) -> Vec<TranscriptEntry> {` |
| `src/harness/transcript.rs:71` | `pub fn parse_claude_transcript_timed(text: &str) -> Vec<TimedEntry> {` |
| `src/chat_page.rs:61` | `pub fn agent_chat_html() -> String {` |
| `src/chat_page.rs:74` | `pub fn user_prompt_payload(text: &str) -> Value {` |
| `src/chat_page.rs:84` | `pub fn harness_payload(event: &HarnessEvent) -> Option<Value> {` |
| `src/chat_page.rs:121` | `pub fn turn_timing(start_ms: Option<i64>, end_ms: Option<i64>) -> Value {` |
| `src/chat_page.rs:133` | `pub fn turn_split_time(split_ms: i64) -> Value {` |
| `src/chat_page.rs:140` | `pub fn history_payloads(entries: Vec<TimedEntry>) -> Vec<Value> {` |
| `src/agentd/phone_http.rs:96` | `pub fn phone_chat_html() -> Result<String, String> {` |
| `src/agentd/phone_http.rs:302` | `pub fn routes(` |
| `src/agentd/phone_http.rs:641` | `fn history_events(session_id: &str) -> Result<Option<Vec<Value>>, String> {` |
| `src/agentd/phone_http.rs:682` | `fn event_frame(event: &SeqEvent, redelivered: bool) -> String {` |
| `src/agentd/phone_http.rs:693` | `async fn phone_socket(state: PhoneState, socket: WebSocket, ip: IpAddr) {` |
| `src/agentd/phone_http.rs:801` | `async fn relay(` |
| `src/agentd/phone_http.rs:968` | `fn phone_command(phone: &PhoneInfo, chat: &Chat, message: &Message) -> Result<PhoneCommand, Value> {` |
| `src/agentd/phone_http.rs:1040` | `fn act(phone: &PhoneInfo, chat: &Chat, message: &Message) -> Option<Value> {` |
| `src/agentd/phone_http.rs:1660` | `fn event_frames_are_json_the_page_reads() {` |
| `src/agentd/claude_chats.rs:60` | `pub struct SeqEvent {` |
| `src/agentd/claude_chats.rs:320` | `pub fn send(&self, command: HarnessCommand) -> Result<(), String> {` |
| `src/agentd/claude_chats.rs:342` | `pub fn events_after(&self, after_seq: u64) -> Result<Vec<SeqEvent>, Gap> {` |
| `src/agentd/claude_chats.rs:460` | `pub struct ChatAttachment {` |
| `src/agentd/claude_chats.rs:473` | `impl Drop for ChatAttachment {` |
| `src/agentd/claude_chats.rs:527` | `pub fn start_or_attach(` |
| `src/agentd/claude_chats.rs:596` | `pub fn attach(&self, chat_id: &str, after_seq: Option<u64>) -> Option<ChatAttachment> {` |

## Other consumers to retain

The base search `git grep -l -E "HarnessEvent|HarnessCommand|TranscriptEntry|AgentSession|history_payloads|spawn_agent_task" f6df7cae36ac8af2ef758b5619a86ef5f56a6972 -- src examples` also finds:

- Remote gRPC/client envelopes: `src/agentd/{server,client}.rs` and `proto/gitterm/agent/v6/agent.proto`.
- Snapshot/fact/worktree observers: `src/{turn_snapshots,chat_facts,worktree_follow}.rs`.
- Derived views/services: `src/{chat_overview,chat_summary,plan_limits_view,delegations,html_render,work_prs}.rs`.
- Session storage/adapters: `src/tab/mod.rs`, `src/config.rs`, `src/chats.rs`, `src/baseline_tests.rs`; `src/main.rs` session persistence and subscription bridges.
- Display/IPC/phone: `assets/agent_chat.html`, `assets/phone/{chat_head,chat_shim}.html`, `src/webview.rs`, `src/chat_page.rs`.
- Examples: `examples/{claude_harness_smoke,claude_harness_mcp_smoke,acp_harness_smoke,remote_claude_chat_smoke,remote_claude_chat_persist_smoke,phone_smoke,delegation_smoke,worker_delegation_smoke}.rs`. These may launch real agents and are deliberately **not run** here.
- Existing serde/transport/parser tests remain acceptance coverage; the new C0 fixtures supplement them with entirely synthetic data.

## Ownership and uncovered seams

`AgentSession::conversation` remains authoritative. Tests call the existing headless `baseline_tests::app_with_tabs` through a child test module registered in `src/tab/agent.rs`; no second reducer or App initializer is added. Picker persistence is confined to an isolated test subprocess.

Native command dispatch from App (submit while running, answer/interrupt/withdraw write acceptance and rejected-write UI recovery) still needs an injectable `HarnessSession` handle accessible to the binary test build. Its library-only `cfg(test)` private constructors cannot be reached by the binary. C0 pins those commands at the actual Claude/ACP process/parser/queue seams and pins App answer-without-process, pending resolution and confirmed-picker updates. No main.rs seam was added.

Phone event envelopes and command rejections are golden-tested via the actual `event_frame`/`act` functions and a fake registry. `opened`/`history`/live/refusal/revocation frames are also golden-pinned by adding assertions to the existing fake-socket test, with a registry barrier before hello. Hello authorization and close-code delivery retain the existing socket tests. The new sequence fixture records actual registry reconnect/gap/redelivery outcomes without opening a socket.
