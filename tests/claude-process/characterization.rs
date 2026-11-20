use super::*;
#[path = "../native-chat/native_chat_process.rs"]
mod process;

// A split JSON line and a final line without a newline both survive a crash.
#[tokio::test]
async fn claude_partial_output_crash_is_reaped_without_resending() {
    let dir = tempfile::tempdir().unwrap();
    let program = process::script(
        dir.path(),
        "fake-partial-claude",
        include_str!("partial.py"),
    );
    let config = ClaudeSessionConfig {
        cwd: dir.path().into(),
        snapshot_facts_dir: None,
        model: None,
        permission_mode: "default".into(),
        effort: None,
        resume: None,
        wire_log_dir: None,
        mcp_servers: Vec::new(),
        chrome: false,
        guidelines_file: None,
        guidelines_content: None,
    };
    let (commands, rx) = mpsc::unbounded_channel();
    let (tx, mut events) = mpsc::unbounded_channel();
    let pid = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let task = tokio::spawn(run_session_program(
        config,
        rx,
        tx,
        tokio_util::sync::CancellationToken::new(),
        program,
        pid.clone(),
    ));
    let mut seen = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .unwrap()
    {
        if matches!(event, HarnessEvent::Ready { .. }) {
            commands
                .send(HarnessCommand::SendUserMessage(UserPrompt::from(
                    "Synthetic prompt",
                )))
                .unwrap();
        } else {
            seen.push(event);
        }
    }
    task.await.unwrap();
    assert_eq!(
        seen,
        vec![
            HarnessEvent::TextDelta("partial output".into()),
            HarnessEvent::TextDelta(" final fragment".into()),
            HarnessEvent::Error("claude exited unexpectedly (code Some(17))".into()),
            HarnessEvent::ProcessExited { code: Some(17) },
        ]
    );
    assert_eq!(pid.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(commands.is_closed());
    process::assert_reaped(
        std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .parse()
            .unwrap(),
    );
}
