//! Native Claude Code session: one long-lived `claude` process driven over
//! the stream-json control protocol in both directions.
//!
//! Wire contract: `.plans/tru-140-claude-control-protocol.md` (Phase A).
//! `ClaudeFrameParser` turns CLI stdout lines into `HarnessEvent`s and is
//! pure (no IO), so it is unit-tested against recorded frames in
//! `tests/fixtures/claude/`. `ClaudeSession` owns the process on a
//! dedicated thread and maps `HarnessCommand`s onto stdin frames.

use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use serde::Deserialize;
#[cfg(test)]
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{HarnessCommand, HarnessEvent};
#[cfg(test)]
use super::{RuntimeDecision, RuntimeRequestKind, UserPrompt};
use crate::agentd::client::{RemoteAgentBackend, RemoteAgentClientConfig};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub use gitterm_core::harness::claude_parser::{
    permission_result, user_message_frame, ClaudeFrameParser, HostRequest, ParsedFrame,
    SnapshotHook,
};

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

use gitterm_core::chat::runtime::Session;
pub use gitterm_core::harness::claude::{
    chrome_flag, configure_terminal_chrome, ClaudeMcpServer, ClaudeSessionConfig,
};
use gitterm_core::harness::claude::{Launch, SnapshotFuture, SnapshotRecorder};

#[cfg(test)]
use gitterm_core::harness::local_launch::child_environment;
pub use gitterm_core::harness::local_launch::resolve_claude_program;

/// Handle to a running Claude session. Dropping a local one shuts the
/// process down; dropping a remote one on an agent that owns its chats
/// only detaches (send `Shutdown` first to stop it).
pub struct ClaudeSession {
    inner: Option<Session>,
}

impl ClaudeSession {
    /// Wrap a host-owned transport port without spawning a local process.
    pub fn from_port(commands: mpsc::UnboundedSender<HarnessCommand>) -> Self {
        Self {
            inner: Some(Session::from_port(commands, false)),
        }
    }

    pub fn spawn(config: ClaudeSessionConfig) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (inner, events) = Session::spawn(move |token| {
            let program = resolve_claude_program()?;
            Ok(local_launch(config, program, token))
        });
        (Self { inner: Some(inner) }, events)
    }
    /// Iced consumes delivery receipts and harness events in the same order.
    pub fn spawn_runtime(
        config: ClaudeSessionConfig,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
    ) {
        let (inner, events) = Session::spawn_runtime(move |token| {
            let program = resolve_claude_program()?;
            Ok(local_launch(config, program, token))
        });
        (Self { inner: Some(inner) }, events)
    }
    /// Returns whether actual write receipts will follow channel acceptance.
    pub fn send_chat(
        &self,
        command: HarnessCommand,
        correlation: gitterm_core::chat::types::CommandCorrelation,
    ) -> Result<bool, String> {
        if self.inner.as_ref().unwrap().supports_correlation() {
            self.inner
                .as_ref()
                .unwrap()
                .send_correlated(command, correlation)
                .map(|()| true)
        } else {
            self.inner.as_ref().unwrap().send(command).map(|()| false)
        }
    }
    pub fn close_turn_snapshots(&self) {
        self.inner.as_ref().unwrap().close_turn_snapshots();
    }
    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.inner.as_ref().unwrap().send(command)
    }
    pub fn local_pid(&self) -> Option<u32> {
        self.inner.as_ref().unwrap().local_pid()
    }
}

// Remote session supervision is shared with other frontends.
pub use gitterm_core::harness::remote_chat::RemoteClaudeChatStart;

impl ClaudeSession {
    /// Core owns launch and supervision; Iced supplies its configured transport.
    pub fn spawn_remote(
        client: RemoteAgentClientConfig,
        config: ClaudeSessionConfig,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (inner, events) = gitterm_core::harness::remote_chat::spawn_remote(
            RemoteAgentBackend::new(client).into_core(),
            config,
        );
        (Self { inner: Some(inner) }, events)
    }

    /// Attach without starting another process; see core's remote_chat adapter.
    pub fn attach_remote(
        client: RemoteAgentClientConfig,
        config: ClaudeSessionConfig,
        chat_id: String,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let (inner, events) = gitterm_core::harness::remote_chat::attach_remote(
            RemoteAgentBackend::new(client).into_core(),
            config,
            chat_id,
        );
        (Self { inner: Some(inner) }, events)
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        let Some(mut session) = self.inner.take() else {
            return;
        };
        session.request_shutdown();
        crate::shutdown::spawn(
            crate::shutdown::Kind::Session,
            "Claude session I/O and snapshots",
            move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("Claude join runtime: {error}"))?;
                runtime.block_on(session.shutdown())?;
                drop(session);
                Ok(())
            },
        );
    }
}

// Keep discovery on the existing session thread. The core receives the same
// rooted Recorder and shares its close token, including its legacy invalid-root
// disabling behavior and private thread-local roots in tests.
fn local_launch(
    config: ClaudeSessionConfig,
    program: PathBuf,
    token: tokio_util::sync::CancellationToken,
) -> Launch {
    let snapshots = crate::turn_snapshots::Recorder::new(
        config.cwd.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(token);
    gitterm_core::harness::local_launch::local_launch(config, program, Box::new(snapshots))
}

impl SnapshotRecorder for crate::turn_snapshots::Recorder {
    fn end(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.end())
    }
    fn observe(&mut self, event: &HarnessEvent) {
        self.observe(event);
    }
    fn invalidate_end(&mut self) {
        self.invalidate_end();
    }
    fn discard_turn(&mut self) {
        self.discard_turn();
    }
    fn bind<'a>(&'a mut self, chat: &'a str) -> SnapshotFuture<'a> {
        Box::pin(self.bind(chat))
    }
    fn begin(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.begin())
    }
    fn capture_end(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.capture_end())
    }
    fn finish(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.finish())
    }
    fn close(&mut self) -> SnapshotFuture<'_> {
        Box::pin(self.close())
    }
}

#[cfg(test)]
async fn run_session_program(
    config: ClaudeSessionConfig,
    commands: mpsc::UnboundedReceiver<HarnessCommand>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    token: tokio_util::sync::CancellationToken,
    program: PathBuf,
    pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let root = config.cwd.clone();
    let snapshots = crate::turn_snapshots::Recorder::new(
        root.clone(),
        config.resume.clone(),
        config.snapshot_facts_dir.clone(),
    )
    .with_close_token(token.clone());
    let launch = Launch {
        flags: Default::default(),
        config,
        program,
        environment: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.join("home").into_os_string()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.join("provider").into_os_string(),
            ),
        ],
        temporary_dir: root,
        snapshots: Box::new(snapshots),
    };
    gitterm_core::harness::claude::run_session_program(launch, commands, events, token, pid).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_child_environment_preserves_legacy_sanitization_and_mcp_order() {
        let inherited = [
            ("HOME", "/synthetic/home"),
            ("CLAUDE_CONFIG_DIR", "/synthetic/provider"),
            ("PATH", "/old/path"),
            ("CLAUDECODE", "parent"),
            ("CLAUDE_CODE_SESSION_ID", "parent"),
            ("CLAUDE_PID", "123"),
            ("NODE_OPTIONS", "parent"),
            ("CLAUDE_CODE_ENTRYPOINT", "parent"),
            ("UNRELATED", "preserved"),
        ]
        .map(|(key, value)| (key.into(), value.into()));
        let servers = ["first", "last"].map(|value| ClaudeMcpServer {
            config: json!({"mcpServers": {}}),
            allowed_tools: Vec::new(),
            env: vec![("SYNTHETIC_TOKEN".into(), value.into())],
            public_env: Vec::new(),
        });
        let env: std::collections::BTreeMap<_, _> =
            child_environment(inherited, "/child/path".into(), &servers)
                .into_iter()
                .collect();
        let expected = [
            ("HOME", "/synthetic/home"),
            ("CLAUDE_CONFIG_DIR", "/synthetic/provider"),
            ("PATH", "/child/path"),
            ("UNRELATED", "preserved"),
            ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
            ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"),
            ("SYNTHETIC_TOKEN", "last"),
        ]
        .map(|(key, value)| (key.into(), value.into()))
        .into_iter()
        .collect();
        assert_eq!(env, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_start_reinjects_workspace_context_after_clear_compact_and_move() {
        check_session_start_workspace_context(true, false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_moved_workspace_prompt_and_session_start_match_v6() {
        for resume in [false, true] {
            check_session_start_workspace_context(false, resume).await;
        }
    }

    #[cfg(unix)]
    async fn check_session_start_workspace_context(move_chat: bool, resume: bool) {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("notes/example-work");
        std::fs::create_dir_all(home.join("docs")).unwrap();
        git2::Repository::init(home.parent().unwrap()).unwrap();
        let home = std::fs::canonicalize(home).unwrap();
        std::fs::write(
            home.join("docs/checkpoint.md"),
            "INITIAL_CHECKPOINT_CONTENT_TRU259",
        )
        .unwrap();
        let places = crate::guidelines::document_places(
            true,
            true,
            &home,
            None,
            "Example workspace",
            &[PathBuf::from("/code/example-api")],
        )
        .unwrap();
        let launch = crate::guidelines::launch_instructions(
            Some(crate::guidelines::Guidelines {
                path: fixture.path().join("guidelines.md"),
                text: "Static change guidelines.".into(),
            }),
            Some(&places),
            true,
            fixture.path(),
        )
        .unwrap();
        // Frozen from origin/v6 (936976d), with TRU-259/265's intentional
        // wording changes. Never-moved chats retain this launch prompt;
        // callbacks add only the fresh checkpoint read instruction.
        let expected = include_str!("../../tests/fixtures/claude/workspace-prompt-v6.md")
            .replace("@HOME@", home.to_str().unwrap());
        assert_eq!(launch.text, expected);
        std::fs::write(home.join("expected-system.md"), expected).unwrap();
        if move_chat {
            std::fs::write(home.join("move-chat"), "").unwrap();
        }
        if resume {
            std::fs::write(home.join("resuming"), "").unwrap();
        }
        let program = fixture.path().join("mock-claude");
        std::fs::write(&program, r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
system_file = Path(sys.argv[sys.argv.index('--append-system-prompt-file') + 1])
system = system_file.read_bytes()
assert system == Path('expected-system.md').read_bytes()
assert ('--resume=example-session' in sys.argv) == Path('resuming').exists()
expected_context = system.decode().split('# Documents for this work (added by GitTerm)\n', 1)[1]
expected_context = '# Documents for this work (added by GitTerm)\n' + expected_context.split('# Starting Codex in GitTerm', 1)[0].rstrip()
expected_context += f'\nRe-read the workspace checkpoint `{Path.cwd() / "docs/checkpoint.md"}` now, before continuing: it may have changed since this chat started.'
def emit(value):
    print(json.dumps(value), flush=True)
def read():
    line = sys.stdin.readline()
    assert line, 'host closed before callback response'
    return json.loads(line)
init = read()
assert init['request']['hooks']['SessionStart'][0]['hookCallbackIds'] == ['gitterm_workspace_context']
assert 'UserPromptSubmit' not in init['request']['hooks'] # independent of snapshots
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{'hooks_applied':True}}})
# Real Claude 2.1.322 sends these callbacks only for clear/compact.
for source in ['clear', 'compact']:
    emit({'type':'control_request','request_id':source,'request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':source,'session_id':'example-session'}}})
    response = read()['response']
    assert response['request_id'] == source
    output = response['response']['hookSpecificOutput']
    assert output['hookEventName'] == 'SessionStart'
    text = output['additionalContext']
    assert 'Workspace: `Example workspace`' in text
    assert '/code/example-api' in text
    assert str(Path.cwd() / 'docs/checkpoint.md') in text
    assert "resume only this workspace's work; if unsure which work to continue, ask." in text
    assert text.count('Re-read the workspace checkpoint') == 1
    assert 'The start of the workspace checkpoint' not in text
    assert 'INITIAL_CHECKPOINT_CONTENT_TRU259' not in text
    assert 'Starting Codex' not in text
    assert text == expected_context
emit({'type':'result','subtype':'success'})
if Path('move-chat').exists():
    read() # the host moves this running chat, then sends /clear
    for source in ['clear', 'compact']:
        emit({'type':'control_request','request_id':'moved-' + source,'request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':source,'session_id':'example-session'}}})
        text = read()['response']['response']['hookSpecificOutput']['additionalContext']
        assert 'Workspace: `Destination workspace`' in text
        assert str(Path.cwd().parent / 'destination/docs/checkpoint.md') in text
        assert 'Destination checkpoint written after move' not in text
        assert 'docs/checkpoint.md` (exists)' in text
        assert text.count('Re-read the workspace checkpoint') == 1
        assert f'Re-read the workspace checkpoint `{Path.cwd().parent / "destination/docs/checkpoint.md"}` now' in text
        assert 'This chat now belongs to the following workspace' in text
        assert 'replaces the workspace section in the system prompt' in text
        assert 'Example workspace' not in text and 'Example work' not in text
        assert system_file.read_bytes() == system
    emit({'type':'result','subtype':'success'})
    read() # moving back to the launch workspace removes the override
    emit({'type':'control_request','request_id':'returned-clear','request':{'subtype':'hook_callback','callback_id':'gitterm_workspace_context','input':{'hook_event_name':'SessionStart','source':'clear','session_id':'example-session'}}})
    assert read()['response']['response']['hookSpecificOutput']['additionalContext'] == expected_context
    emit({'type':'result','subtype':'success'})
for line in sys.stdin:
    pass
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_session_program(
            ClaudeSessionConfig {
                cwd: home.clone(),
                snapshot_facts_dir: None,
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: resume.then(|| "example-session".into()),
                wire_log_dir: None,
                mcp_servers: Vec::new(),
                chrome: false,
                guidelines_file: Some(launch.path),
                guidelines_content: None,
            },
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
            program,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let destination = home.parent().unwrap().join("destination");
        std::fs::create_dir_all(destination.join("docs")).unwrap();
        let places = crate::guidelines::document_places(
            true,
            true,
            &destination,
            None,
            "Destination workspace",
            &[],
        )
        .unwrap();
        let mut completed = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                HarnessEvent::TurnCompleted { .. } if move_chat && completed == 0 => {
                    commands
                        .send(HarnessCommand::SetWorkspaceContext(Some(
                            crate::guidelines::documents_note(&places),
                        )))
                        .unwrap();
                    std::fs::write(
                        destination.join("docs/checkpoint.md"),
                        "Destination checkpoint written after move",
                    )
                    .unwrap();
                    commands
                        .send(HarnessCommand::SendUserMessage(UserPrompt::from("/clear")))
                        .unwrap();
                    completed += 1;
                }
                HarnessEvent::TurnCompleted { .. } if move_chat && completed == 1 => {
                    commands
                        .send(HarnessCommand::SetWorkspaceContext(Some(
                            launch.text.clone(),
                        )))
                        .unwrap();
                    commands
                        .send(HarnessCommand::SendUserMessage(UserPrompt::from("/clear")))
                        .unwrap();
                    completed += 1;
                }
                HarnessEvent::TurnCompleted { .. } => break,
                HarnessEvent::Error(error) => panic!("mock SessionStart callback: {error}"),
                HarnessEvent::ProcessExited { code } => {
                    panic!("callback mock exited early: {code:?}")
                }
                _ => {}
            }
        }
        commands.send(HarnessCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_owned_queued_turn_waits_for_snapshot_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let facts = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-qb", "main"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(crate::agentd::git::git_command()
                .current_dir(repo.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let program = facts.path().join("claude-mock");
        std::fs::write(&program, r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
def emit(x):
    print(json.dumps(x), flush=True)
def read():
    return json.loads(sys.stdin.readline())
init = read()
hooks = init['request']['hooks']
emit({'type':'control_response','response':{'subtype':'success','request_id':init['request_id'],'response':{'hooks_applied':True}}})
def boundary(event='UserPromptSubmit'):
    matchers = hooks.get(event, [])
    if matchers:
        callback = matchers[0]['hookCallbackIds'][0]
        emit({'type':'control_request','request_id':'boundary','request':{'subtype':'hook_callback','callback_id':callback,'input':{'hook_event_name':event,'session_id':'queued-chat'}}})
        reply = read()
        assert reply['response']['request_id'] == 'boundary'
read() # first user message
boundary()
emit({'type':'system','subtype':'init','session_id':'queued-chat','model':'fixture'})
read() # second user message queued in the CLI, not the host
boundary() # prompt hooks may run at submission, while turn one is active
boundary('PreToolUse')
Path('file.txt').write_text('one')
boundary('Stop')
emit({'type':'result','subtype':'success'})
# No sleeping: only the callback response can prevent racing capture.
boundary('PreToolUse')
Path('file.txt').write_text('two')
boundary('Stop')
emit({'type':'result','subtype':'success'})
for line in sys.stdin:
    pass
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (commands, rx) = mpsc::unbounded_channel();
        let (tx, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_session_program(
            ClaudeSessionConfig {
                cwd: repo.path().into(),
                snapshot_facts_dir: Some(facts.path().into()),
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: None,
                wire_log_dir: None,
                mcp_servers: Vec::new(),
                chrome: false,
                guidelines_file: None,
                guidelines_content: None,
            },
            rx,
            tx,
            tokio_util::sync::CancellationToken::new(),
            program,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let mut completed = 0;
        while completed < 2 {
            let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                HarnessEvent::Ready { .. } => commands
                    .send(HarnessCommand::SendUserMessage(UserPrompt::from("one")))
                    .unwrap(),
                HarnessEvent::TurnStarted { .. } => commands
                    .send(HarnessCommand::SendUserMessage(UserPrompt::from("two")))
                    .unwrap(),
                HarnessEvent::TurnCompleted { .. } => completed += 1,
                HarnessEvent::Error(e) => panic!("mock session: {e}"),
                _ => {}
            }
        }
        commands.send(HarnessCommand::Shutdown).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let turns = crate::turn_snapshots::turns(repo.path(), "queued-chat")
            .await
            .unwrap();
        assert_eq!(
            turns.len(),
            2,
            "each CLI-owned turn needs its own snapshots"
        );
        let store = crate::turn_snapshots::store_path(repo.path())
            .await
            .unwrap();
        let show = |oid: &str| {
            let output = crate::agentd::git::git_command()
                .current_dir(&store)
                .args(["show", &format!("{oid}:file.txt")])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(show(turns[1].end.as_deref().unwrap()), "one");
        assert_eq!(show(turns[0].start.as_deref().unwrap()), "one");
        assert_eq!(show(turns[0].end.as_deref().unwrap()), "two");
    }

    #[test]
    fn args_follow_the_phase_a_contract() {
        let cfg = ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd: PathBuf::from("/tmp"),
            model: Some("haiku".into()),
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: true,
            guidelines_file: None,
            guidelines_content: None,
        };
        let args = cfg.args().join(" ");
        assert!(cfg.args().contains(&"--chrome".to_string()));
        assert!(!cfg.args().contains(&"--no-chrome".to_string()));
        let off = ClaudeSessionConfig {
            snapshot_facts_dir: None,
            chrome: false,
            ..cfg.clone()
        }
        .args();
        assert!(off.contains(&"--no-chrome".to_string()));
        assert!(!off.contains(&"--chrome".to_string()));
        assert!(args.contains("--permission-prompt-tool stdio"));
        assert!(args.contains("--input-format stream-json"));
        assert!(args.contains("--include-partial-messages"));
        assert!(args.contains("--model haiku"));
        assert!(args.contains("--permission-mode default"));
        assert!(args.ends_with("--resume=sid"));
        assert!(!args.contains("--print"));
        assert!(!args.contains("--setting-sources"));
    }

    #[test]
    fn a_resumed_or_remote_chat_carries_the_chrome_choice() {
        // A resume (a chat reopened from the Chats panel or a restart).
        let resumed = config_with(Vec::new());
        assert!(resumed.args().contains(&"--no-chrome".to_string()));
        assert!(resumed.args().contains(&"--resume=sid".to_string()));
        assert!(ClaudeSessionConfig {
            snapshot_facts_dir: None,
            chrome: true,
            ..resumed.clone()
        }
        .args()
        .contains(&"--chrome".to_string()));
        // A remote chat: the agent side starts with the same option.
        let start = RemoteClaudeChatStart::from_config(&resumed).unwrap();
        let agent = start.into_config(PathBuf::from("/tmp"));
        assert!(agent.args().contains(&"--no-chrome".to_string()));
        // An agent started by a build before TRU-181 sent `chrome: false`.
        let older: RemoteClaudeChatStart = serde_json::from_value(json!({
            "cwd": "/tmp", "model": null, "permission_mode": "default",
            "effort": null, "resume": null, "chrome": false
        }))
        .unwrap();
        let older_config = older.into_config(PathBuf::from("/tmp"));
        assert!(older_config.args().contains(&"--no-chrome".to_string()));
        assert!(older_config.guidelines_content.is_none());
    }

    #[test]
    fn terminal_claude_launches_get_the_chrome_option() {
        assert_eq!(
            configure_terminal_chrome("claude", false),
            "claude --no-chrome"
        );
        assert_eq!(configure_terminal_chrome("claude", true), "claude --chrome");
        // Resumes and worker launches with a brief.
        assert_eq!(
            configure_terminal_chrome("claude --resume abc", false),
            "claude --no-chrome --resume abc"
        );
        assert_eq!(
            configure_terminal_chrome("claude --session-id abc \"$(cat '/b.md')\"", false),
            "claude --no-chrome --session-id abc \"$(cat '/b.md')\""
        );
        assert_eq!(
            configure_terminal_chrome("/opt/bin/claude --model opus", true),
            "/opt/bin/claude --chrome --model opus"
        );
        assert_eq!(
            configure_terminal_chrome("'/opt/bin/claude' --resume abc", false),
            "'/opt/bin/claude' --no-chrome --resume abc"
        );
        // A command that already chooses keeps its choice.
        assert_eq!(
            configure_terminal_chrome("claude --chrome --resume abc", false),
            "claude --chrome --resume abc"
        );
        assert_eq!(
            configure_terminal_chrome("claude --no-chrome", true),
            "claude --no-chrome"
        );
        // Other harnesses and commands are untouched.
        for command in ["codex resume --last", "pi --model x", "npm run dev", ""] {
            assert_eq!(configure_terminal_chrome(command, false), command);
        }
        assert_eq!(
            configure_terminal_chrome("claude-helper --x", false),
            "claude-helper --x"
        );
    }

    const SECRET: &str = "s3cret-bearer-value";

    fn mcp_server(name: &str, token_env: &str, allowed: &[&str]) -> ClaudeMcpServer {
        ClaudeMcpServer {
            config: json!({
                "mcpServers": {
                    name: {
                        "type": "http",
                        "url": "http://127.0.0.1:1/mcp",
                        "headers": { "Authorization": format!("Bearer ${{{token_env}}}") },
                    },
                },
            }),
            allowed_tools: allowed.iter().map(|tool| tool.to_string()).collect(),
            env: vec![(token_env.to_string(), SECRET.to_string())],
            public_env: Vec::new(),
        }
    }

    fn config_with(mcp_servers: Vec<ClaudeMcpServer>) -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            snapshot_facts_dir: None,
            cwd: PathBuf::from("/tmp"),
            model: None,
            permission_mode: "default".into(),
            effort: None,
            resume: Some("sid".into()),
            wire_log_dir: None,
            mcp_servers,
            chrome: false,
            guidelines_file: None,
            guidelines_content: None,
        }
    }

    #[test]
    fn remote_start_carries_the_config_and_round_trips() {
        let mut config = config_with(Vec::new());
        config.model = Some("haiku".into());
        config.effort = Some("low".into());
        config.chrome = true;
        config.wire_log_dir = Some(PathBuf::from("/tmp/wire"));
        let start = RemoteClaudeChatStart::from_config(&config).unwrap();
        assert_eq!(
            serde_json::to_value(&start).unwrap(),
            json!({"cwd": "/tmp", "model": "haiku", "permission_mode": "default",
                   "effort": "low", "resume": "sid", "chrome": true})
        );
        let back: RemoteClaudeChatStart =
            serde_json::from_str(&serde_json::to_string(&start).unwrap()).unwrap();
        assert_eq!(back, start);
        // The agent side never gets a wire log or MCP servers.
        let agent = back.into_config(PathBuf::from("/private/tmp"));
        assert_eq!(agent.cwd, PathBuf::from("/private/tmp"));
        assert_eq!(agent.wire_log_dir, None);
        assert!(agent.mcp_servers.is_empty());
        assert_eq!(agent.args(), {
            let mut local = config.clone();
            local.wire_log_dir = None;
            local.args()
        });
    }

    #[test]
    fn remote_start_carries_guideline_text_and_old_payloads_still_decode() {
        let dir = tempfile::tempdir().unwrap();
        let guidelines_path = dir.path().join("agent-guidelines.md");
        std::fs::write(&guidelines_path, "Shared guideline text.\n").unwrap();
        let mut config = config_with(Vec::new());
        config.guidelines_file = Some(guidelines_path);
        let start = RemoteClaudeChatStart::from_config(&config).unwrap();
        assert_eq!(
            start.guidelines.as_deref(),
            Some("Shared guideline text.\n")
        );
        let payload = serde_json::to_value(&start).unwrap();
        assert_eq!(payload["guidelines"], "Shared guideline text.\n");
        #[derive(Deserialize)]
        struct OlderAgentStart {
            cwd: String,
            model: Option<String>,
            permission_mode: String,
            effort: Option<String>,
            resume: Option<String>,
            chrome: bool,
        }
        let older_agent: OlderAgentStart = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(older_agent.cwd, "/tmp");
        assert_eq!(older_agent.permission_mode, "default");
        assert!(older_agent.model.is_none());
        assert!(older_agent.effort.is_none());
        assert_eq!(older_agent.resume.as_deref(), Some("sid"));
        assert!(!older_agent.chrome);
        let back: RemoteClaudeChatStart = serde_json::from_value(payload).unwrap();
        let remote = back.into_config(PathBuf::from("/tmp"));
        assert_eq!(
            remote.guidelines_content.as_deref(),
            Some("Shared guideline text.\n")
        );

        let older: RemoteClaudeChatStart = serde_json::from_value(json!({
            "cwd": "/tmp", "model": null, "permission_mode": "default",
            "effort": null, "resume": null, "chrome": false
        }))
        .unwrap();
        assert!(older.guidelines.is_none());
        assert!(older
            .into_config(PathBuf::from("/tmp"))
            .guidelines_content
            .is_none());
    }

    #[test]
    fn remote_start_refuses_mcp_servers() {
        let config = config_with(vec![mcp_server("gitterm-tasks", "TOKEN", &[])]);
        let err = RemoteClaudeChatStart::from_config(&config).unwrap_err();
        assert!(err.contains("remote machine"), "{err}");
    }

    fn flag_values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        let prefix = format!("{flag}=");
        args.iter()
            .filter_map(|arg| arg.strip_prefix(prefix.as_str()))
            .collect()
    }

    #[test]
    fn args_without_mcp_servers_have_no_mcp_flags() {
        let args = config_with(Vec::new()).args();
        assert!(args
            .iter()
            .all(|arg| !arg.starts_with("--mcp-config") && !arg.starts_with("--allowedTools")));
    }

    #[test]
    fn args_attach_one_mcp_server_with_the_token_left_in_the_environment() {
        let args = config_with(vec![mcp_server(
            "gitterm_tasks",
            "TASK_TOKEN",
            &["mcp__gitterm_tasks"],
        )])
        .args();
        let configs = flag_values(&args, "--mcp-config");
        assert_eq!(configs.len(), 1);
        let config: Value = serde_json::from_str(configs[0]).expect("config is one JSON value");
        assert_eq!(
            config["mcpServers"]["gitterm_tasks"]["headers"]["Authorization"],
            "Bearer ${TASK_TOKEN}"
        );
        assert_eq!(flag_values(&args, "--allowedTools"), ["mcp__gitterm_tasks"]);
        assert_eq!(args.last().map(String::as_str), Some("--resume=sid"));
        assert!(args.iter().all(|arg| !arg.contains(SECRET)));
    }

    #[test]
    fn args_attach_two_mcp_servers_as_repeated_flags() {
        let servers = vec![
            mcp_server("gitterm_tasks", "TASK_TOKEN", &["mcp__gitterm_tasks"]),
            mcp_server(
                "gitterm_browser",
                "BROWSER_TOKEN",
                &[
                    "mcp__gitterm_browser__browser_status",
                    "mcp__gitterm_browser__browser_snapshot",
                ],
            ),
        ];
        let cfg = config_with(servers);
        let args = cfg.args();
        let names: Vec<String> = flag_values(&args, "--mcp-config")
            .into_iter()
            .map(|raw| {
                let config: Value = serde_json::from_str(raw).expect("config is one JSON value");
                let servers = config["mcpServers"].as_object().expect("mcpServers object");
                assert_eq!(servers.len(), 1);
                servers.keys().next().cloned().unwrap_or_default()
            })
            .collect();
        assert_eq!(names, ["gitterm_tasks", "gitterm_browser"]);
        assert_eq!(
            flag_values(&args, "--allowedTools"),
            [
                "mcp__gitterm_tasks",
                "mcp__gitterm_browser__browser_status,mcp__gitterm_browser__browser_snapshot",
            ]
        );
        assert!(args.iter().all(|arg| !arg.contains(SECRET)));
        assert!(
            !format!("{cfg:?}").contains(SECRET),
            "Debug must redact env values"
        );
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)] // Shared characterization sources are loaded by multiple focused test modules.
#[path = "../../tests/native-chat/native_chat_claude.rs"]
mod native_chat_tests;

#[cfg(all(test, unix))]
#[allow(clippy::duplicate_mod)] // Reuses the process fixture from native chat characterization tests.
#[path = "../../tests/claude-process/characterization.rs"]
mod process_characterization;
