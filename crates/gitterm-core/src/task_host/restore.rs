//! What a restored task terminal runs (Iced's relaunch restore, moved here
//! from src/main.rs so the GPUI shell restores the same command).
//!
//! A task tab reopens its recorded conversation: relaunching the persisted
//! plain harness command would fork a fresh session and flip the task back
//! to Running, hiding the Resume verb. Without a recorded conversation, a
//! Claude resume whose transcript was never written starts a fresh `claude`,
//! and a pre-assigned `--session-id` launch whose session has since been
//! written resumes it.
use crate::tasks::{HarnessConversationBackend, HarnessConversationRef, HarnessKind, TaskRecord};

/// The command a restored tab starts, and the conversation it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredStartup {
    pub startup_command: Option<String>,
    pub chat_session_id: Option<String>,
}

/// Iced's restore rewrite of a task tab's saved `startup_command`.
///
/// `task` is the tab's task record (by its saved `task_id`) and
/// `task_session_id` its saved task session. `preset_backend` names the
/// conversation backend of a terminal preset by name, `claude_session_exists`
/// says whether a Claude transcript was written, and `codex_notify` adds the
/// Codex notify hook to a Codex resume. A tab without a saved command keeps
/// what it had.
pub fn restored_startup(
    startup_command: Option<String>,
    chat_session_id: Option<String>,
    task: Option<&TaskRecord>,
    task_session_id: Option<&str>,
    preset_backend: impl Fn(&str) -> Option<HarnessConversationBackend>,
    claude_session_exists: impl Fn(&str) -> bool,
    codex_notify: impl FnOnce(String) -> String,
) -> RestoredStartup {
    let mut restored = RestoredStartup {
        startup_command,
        chat_session_id,
    };
    let Some(saved) = restored.startup_command.as_deref() else {
        return restored;
    };
    let conversation = task.and_then(|task| {
        let own_session = task
            .sessions
            .iter()
            .find(|session| Some(session.task_session_id.as_str()) == task_session_id);
        own_session
            .and_then(|session| session.conversation.as_ref())
            .or_else(|| {
                // pi/codex conversations are linked to synthetic session
                // records by the Chats sync; fall back to the task's latest
                // conversation of this tab's own backend, so a Codex tab
                // never relaunches another session's Claude resume.
                let own_backend = own_session.and_then(|session| {
                    match session.harness.as_ref().map(|harness| &harness.kind) {
                        Some(HarnessKind::TerminalPreset { preset_name }) => {
                            preset_backend(preset_name)
                        }
                        Some(HarnessKind::NativeClaude) => Some(HarnessConversationBackend::Claude),
                        Some(HarnessKind::NativePi) => Some(HarnessConversationBackend::Pi),
                        None => None,
                    }
                })?;
                task.sessions.iter().rev().find_map(|session| {
                    session
                        .conversation
                        .as_ref()
                        .filter(|conversation| conversation.backend == own_backend)
                })
            })
    });
    if let Some(conversation) = conversation {
        let mut command = conversation_resume_command(conversation);
        if conversation.backend == HarnessConversationBackend::Codex {
            command = codex_notify(command);
        }
        restored.startup_command = Some(command);
        restored.chat_session_id = Some(conversation.session_id.clone());
    } else if let Some(session_id) = saved.strip_prefix("claude --resume ").map(str::trim) {
        // A persisted resume against a Claude session that never received a
        // message (the transcript is only written on first message) errors
        // on launch; start a fresh session instead.
        if !session_id.contains(char::is_whitespace) && !claude_session_exists(session_id) {
            restored.startup_command = Some("claude".to_string());
            restored.chat_session_id = None;
        }
    } else if let Some((resume, session_id)) =
        resume_for_preassigned_claude_launch(saved, &claude_session_exists)
    {
        // A plain `claude --session-id X` launch whose session has since been
        // written cannot be re-run (the CLI refuses: "Session ID X is already
        // in use"); resume it instead.
        restored.startup_command = Some(resume);
        restored.chat_session_id = Some(session_id);
    }
    restored
}

/// Shell command that reopens a recorded task conversation with its full
/// history. Derived from the task store alone: the Chats index is only built
/// when the Chats panel opens, so resume must not depend on it.
pub fn conversation_resume_command(conversation: &HarnessConversationRef) -> String {
    let id = &conversation.session_id;
    match conversation.backend {
        HarnessConversationBackend::Claude => format!("claude --resume {id}"),
        HarnessConversationBackend::Codex => format!("codex resume {id}"),
        // pi resumes a specific session via --session (`--resume` is the
        // interactive picker).
        HarnessConversationBackend::Pi => format!("pi --session {id}"),
    }
}

/// A persisted `claude … --session-id <id>` launch whose session has since
/// been written cannot be re-run: the CLI refuses with "Session ID <id> is
/// already in use". Returns the resume command (other flags kept) and the
/// id in that case. None leaves the command alone: not a Claude launch, no
/// `--session-id`, a quoted command (an embedded brief) or a session that
/// never got a message, where the fresh launch is still right.
pub fn resume_for_preassigned_claude_launch(
    command: &str,
    session_exists: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    if command.contains(['"', '\'']) {
        return None;
    }
    let mut words = command.split_whitespace();
    if words.next() != Some("claude") {
        return None;
    }
    let words: Vec<&str> = words.collect();
    let idx = words.iter().position(|w| *w == "--session-id")?;
    let id = *words.get(idx + 1)?;
    if !session_exists(id) {
        return None;
    }
    let mut out = vec!["claude"];
    out.extend(
        words
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx && *i != idx + 1)
            .map(|(_, w)| *w),
    );
    out.push("--resume");
    out.push(id);
    Some((out.join(" "), id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{HarnessSelection, ObjectiveDeliveryState, TaskSessionRecord};
    use std::path::Path;

    fn session(
        id: &str,
        harness: Option<HarnessKind>,
        conversation: Option<(HarnessConversationBackend, &str)>,
    ) -> TaskSessionRecord {
        TaskSessionRecord {
            task_session_id: id.to_string(),
            label: id.to_string(),
            harness: harness.map(|kind| HarnessSelection { kind, model: None }),
            conversation: conversation.map(|(backend, session_id)| HarnessConversationRef {
                backend,
                session_id: session_id.to_string(),
            }),
            objective_delivery: ObjectiveDeliveryState::Unknown,
            launch_identity: None,
            created_at: "2026-11-22T10:00:00Z".to_string(),
            updated_at: "2026-11-22T10:00:00Z".to_string(),
        }
    }

    fn restore(
        command: &str,
        task: Option<&TaskRecord>,
        task_session_id: Option<&str>,
        exists: &[&str],
    ) -> RestoredStartup {
        restored_startup(
            Some(command.to_string()),
            None,
            task,
            task_session_id,
            |name| (name == "Codex").then_some(HarnessConversationBackend::Codex),
            |id| exists.contains(&id),
            |command| format!("{command} -c notify"),
        )
    }

    fn restored(command: &str, chat: Option<&str>) -> RestoredStartup {
        RestoredStartup {
            startup_command: Some(command.to_string()),
            chat_session_id: chat.map(str::to_string),
        }
    }

    #[test]
    fn a_recorded_conversation_is_resumed() {
        use HarnessConversationBackend::{Claude, Codex, Pi};
        let mut task = crate::delegation_test_support::test_task("t", Path::new("/wt/t"));
        task.sessions = vec![
            session("claude", None, Some((Claude, "c-1"))),
            session("pi", None, Some((Pi, "p-1"))),
            session("codex-old", None, Some((Codex, "x-1"))),
            // A Codex preset whose own record has no conversation: the
            // task's latest Codex one, never the Claude one.
            session(
                "codex",
                Some(HarnessKind::TerminalPreset {
                    preset_name: "Codex".into(),
                }),
                None,
            ),
            session("plain", None, None),
        ];
        let task = Some(&task);
        assert_eq!(
            restore("claude --session-id c-1", task, Some("claude"), &[]),
            restored("claude --resume c-1", Some("c-1"))
        );
        assert_eq!(
            restore("pi", task, Some("pi"), &[]),
            restored("pi --session p-1", Some("p-1"))
        );
        assert_eq!(
            restore("codex", task, Some("codex"), &[]),
            restored("codex resume x-1 -c notify", Some("x-1"))
        );
        // No conversation and no backend to fall back on: unchanged.
        assert_eq!(
            restore("codex --yolo", task, Some("plain"), &[]),
            restored("codex --yolo", None)
        );
        // No saved command: nothing starts.
        let none = restored_startup(None, None, task, Some("claude"), |_| None, |_| true, |c| c);
        assert_eq!(none.startup_command, None);
    }

    #[test]
    fn claude_resume_without_a_transcript_starts_fresh() {
        assert_eq!(
            restore("claude --resume abc", None, None, &[]),
            restored("claude", None)
        );
        assert_eq!(
            restore("claude --resume abc", None, None, &["abc"]),
            restored("claude --resume abc", None)
        );
        // Not a bare resume: left as saved.
        assert_eq!(
            restore("claude --resume abc --model opus", None, None, &[]),
            restored("claude --resume abc --model opus", None)
        );
    }

    #[test]
    fn a_written_preassigned_session_is_resumed() {
        assert_eq!(
            restore("claude --model opus --session-id abc", None, None, &["abc"]),
            restored("claude --model opus --resume abc", Some("abc"))
        );
        assert_eq!(
            restore("claude --session-id new", None, None, &["abc"]),
            restored("claude --session-id new", None)
        );
    }

    #[test]
    fn preassigned_claude_launch_resumes_once_the_session_exists() {
        let exists = |id: &str| id == "abc";
        assert_eq!(
            resume_for_preassigned_claude_launch("claude --session-id abc", exists),
            Some(("claude --resume abc".to_string(), "abc".to_string()))
        );
        assert_eq!(
            resume_for_preassigned_claude_launch("claude --resume abc", exists),
            None
        );
        assert_eq!(
            resume_for_preassigned_claude_launch("codex --session-id abc", exists),
            None
        );
        // An embedded brief is quoted; leave it to the task restore path.
        assert_eq!(
            resume_for_preassigned_claude_launch(
                "claude --session-id abc \"$(cat 'b.md')\"",
                exists
            ),
            None
        );
    }
}
