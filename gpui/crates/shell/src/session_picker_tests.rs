use super::*;

fn inputs() -> PickerInputs {
    PickerInputs {
        chat: ChatSelection {
            model: "sonnet".into(),
            effort: Some("high".into()),
        },
        local_workspace: true,
        // Deliberately reversed: Iced's ALL order is authoritative.
        acp_chats: vec![
            AcpChat {
                id: AcpAgent::OpenCode,
                model: String::new(),
            },
            AcpChat {
                id: AcpAgent::Antigravity,
                model: "agy-model".into(),
            },
            AcpChat {
                id: AcpAgent::Gemini,
                model: "legacy".into(),
            },
        ],
        presets: vec![
            AgentPreset {
                name: "Claude".into(),
                command: "claude".into(),
                resume_command: Some("claude --resume".into()),
                icon: "✳".into(),
                color: WorkspaceColor::Peach,
            },
            AgentPreset {
                name: "Codex".into(),
                command: "codex".into(),
                resume_command: Some("codex resume".into()),
                icon: String::new(),
                color: WorkspaceColor::Blue,
            },
            AgentPreset {
                name: "Pi".into(),
                command: "pi".into(),
                resume_command: None,
                icon: String::new(),
                color: WorkspaceColor::Green,
            },
        ],
        task: None,
    }
}

fn opened(inputs: PickerInputs) -> PickerModel {
    let mut model = PickerModel::new(inputs);
    model.open();
    model
}

fn labels(model: &PickerModel) -> Vec<String> {
    model
        .entries()
        .iter()
        .map(|entry| match entry {
            PickerEntry::TaskHeader { title, .. } => format!("CONTINUE TASK: {title}"),
            PickerEntry::Section(label) => (*label).into(),
            PickerEntry::Row(row) => row.label.clone(),
        })
        .collect()
}

fn rows(model: &PickerModel) -> Vec<PickerRow> {
    model
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            PickerEntry::Row(row) => Some(row),
            _ => None,
        })
        .collect()
}

#[test]
fn normal_root_matches_iced_labels_descriptions_icons_and_order() {
    let model = opened(inputs());
    assert_eq!(model.affordance_label(), "+ Session");
    assert_eq!(
        labels(&model),
        [
            "New chat",
            "New Antigravity chat",
            "New OpenCode chat",
            "CLI ›",
            "Terminal"
        ]
    );
    let rows = rows(&model);
    assert_eq!(
        rows.iter()
            .map(|row| row.description.as_str())
            .collect::<Vec<_>>(),
        [
            "Claude · sonnet · high effort",
            "Antigravity · agy-model",
            "OpenCode · its own default model",
            "Claude, Codex, Pi",
            "Plain shell",
        ]
    );
    assert_eq!(
        rows.iter().map(|row| row.icon.as_str()).collect::<Vec<_>>(),
        ["✳", "✧", "◇", "❯", "▸"]
    );
    assert_eq!(rows[3].action, RowAction::OpenCli);
}

#[test]
fn cli_submenu_interleaves_resumes_and_back_returns_to_root() {
    let mut model = opened(inputs());
    assert_eq!(model.activate(3), None);
    assert!(model.is_visible());
    assert_eq!(
        labels(&model),
        [
            "‹ CLI",
            "Claude",
            "Claude (resume)",
            "Codex",
            "Codex (resume)",
            "Pi"
        ]
    );
    let rows = rows(&model);
    assert_eq!(
        rows.iter()
            .map(|row| row.description.as_str())
            .collect::<Vec<_>>(),
        [
            "Back",
            "claude",
            "claude --resume",
            "codex",
            "codex resume",
            "pi"
        ]
    );
    assert_eq!(
        rows.iter().map(|row| row.icon.as_str()).collect::<Vec<_>>(),
        ["", "✳", "↺", "C", "↺", "P"]
    );
    assert_eq!(model.activate(0), None);
    assert_eq!(labels(&model)[0], "New chat");
}

#[test]
fn task_context_has_header_presets_and_tools_without_chat_back_or_resumes() {
    let mut data = inputs();
    data.task = Some(TaskContext {
        task_id: "task-1".into(),
        title: "Implement parity".into(),
        visible_session_count: 0,
        handoff_copied: true,
    });
    let model = opened(data.clone());
    assert_eq!(model.affordance_label(), "Start with ▾");
    assert_eq!(
        labels(&model),
        [
            "CONTINUE TASK: Implement parity",
            "AGENTS",
            "Claude",
            "Codex",
            "Pi",
            "TOOLS",
            "Terminal"
        ]
    );
    assert_eq!(
        model.entries()[0],
        PickerEntry::TaskHeader {
            title: "Implement parity".into(),
            handoff_copied: true
        }
    );
    assert_eq!(
        rows(&model)
            .iter()
            .map(|row| row.description.as_str())
            .collect::<Vec<_>>(),
        ["claude", "codex", "pi", "Plain shell"]
    );
    data.task.as_mut().unwrap().visible_session_count = 1;
    data.task.as_mut().unwrap().handoff_copied = false;
    let model = opened(data);
    assert_eq!(model.affordance_label(), "+ Continue ▾");
    assert!(matches!(
        model.entries()[0],
        PickerEntry::TaskHeader {
            handoff_copied: false,
            ..
        }
    ));
}

#[test]
fn every_normal_entry_emits_its_exact_outcome_and_closes() {
    let root = [
        (0, SessionPickerOutcome::NewChat),
        (
            1,
            SessionPickerOutcome::AcpChat {
                id: AcpAgent::Antigravity,
            },
        ),
        (
            2,
            SessionPickerOutcome::AcpChat {
                id: AcpAgent::OpenCode,
            },
        ),
        (4, SessionPickerOutcome::Terminal),
    ];
    let cli = [
        (1, SessionPickerOutcome::Cli { preset: 0 }),
        (
            2,
            SessionPickerOutcome::CliResume {
                preset: 0,
                command: "claude --resume".into(),
            },
        ),
        (3, SessionPickerOutcome::Cli { preset: 1 }),
        (
            4,
            SessionPickerOutcome::CliResume {
                preset: 1,
                command: "codex resume".into(),
            },
        ),
        (5, SessionPickerOutcome::Cli { preset: 2 }),
    ];
    for (submenu, cases) in [(false, root.as_slice()), (true, cli.as_slice())] {
        for (index, outcome) in cases {
            let mut model = opened(inputs());
            if submenu {
                model.activate(3);
            }
            assert_eq!(
                model.activate(*index),
                Some(SessionPickerEvent::Selected(outcome.clone()))
            );
            assert!(!model.is_visible());
            assert_eq!(model.activate(*index), None);
        }
    }
}

#[test]
fn task_rows_emit_continue_with_task_identity_and_preset_or_terminal() {
    for (index, preset) in [(2, Some(0)), (3, Some(1)), (4, Some(2)), (6, None)] {
        let mut data = inputs();
        data.task = Some(TaskContext {
            task_id: "task-1".into(),
            title: "Task".into(),
            visible_session_count: 1,
            handoff_copied: false,
        });
        let mut model = opened(data);
        for header in [0, 1, 5, usize::MAX] {
            assert_eq!(model.activate(header), None);
            assert!(model.is_visible());
        }
        assert_eq!(
            model.activate(index),
            Some(SessionPickerEvent::Selected(
                SessionPickerOutcome::Continue {
                    task_id: "task-1".into(),
                    preset
                }
            ))
        );
        assert!(!model.is_visible());
    }
}

#[test]
fn remote_and_unavailable_acp_chats_are_omitted_and_empty_cli_remains_enabled() {
    let mut data = inputs();
    data.local_workspace = false;
    data.presets.clear();
    data.chat.effort = None;
    let mut model = opened(data);
    assert_eq!(labels(&model), ["New chat", "CLI ›", "Terminal"]);
    assert_eq!(rows(&model)[0].description, "Claude · sonnet");
    assert_eq!(rows(&model)[1].description, "No agent presets configured");
    assert_eq!(model.activate(1), None);
    assert_eq!(labels(&model), ["‹ CLI"]);
    model.activate(0);
    assert_eq!(labels(&model), ["New chat", "CLI ›", "Terminal"]);
    let mut data = inputs();
    data.acp_chats.retain(|chat| chat.id == AcpAgent::OpenCode);
    assert_eq!(
        labels(&opened(data)),
        ["New chat", "New OpenCode chat", "CLI ›", "Terminal"]
    );
}

#[test]
fn escape_dismisses_even_in_cli_other_keys_do_not_select_and_reopen_resets() {
    let mut model = opened(inputs());
    model.activate(3);
    for key in ["up", "down", "left", "right", "enter", "space", "tab"] {
        assert_eq!(model.key(key), None);
        assert!(model.is_visible());
        assert_eq!(labels(&model)[0], "‹ CLI");
    }
    assert_eq!(model.key("escape"), Some(SessionPickerEvent::Dismissed));
    assert!(!model.is_visible());
    assert_eq!(model.key("escape"), None);
    model.open();
    assert_eq!(labels(&model)[0], "New chat");
    assert_eq!(model.dismiss(), Some(SessionPickerEvent::Dismissed));
    assert_eq!(model.dismiss(), None);
}

#[test]
fn empty_preset_names_and_empty_resume_commands_follow_iced() {
    let mut data = inputs();
    data.presets = vec![AgentPreset {
        name: String::new(),
        command: String::new(),
        resume_command: Some(String::new()),
        icon: String::new(),
        color: WorkspaceColor::Teal,
    }];
    let mut model = opened(data);
    model.activate(3);
    assert_eq!(rows(&model)[1].icon, "?");
    assert_eq!(labels(&model), ["‹ CLI", "", " (resume)"]);
    assert_eq!(
        model.activate(2),
        Some(SessionPickerEvent::Selected(
            SessionPickerOutcome::CliResume {
                preset: 0,
                command: String::new()
            }
        ))
    );
}

#[test]
fn task_without_presets_keeps_its_terminal_available() {
    let mut data = inputs();
    data.presets.clear();
    data.task = Some(TaskContext {
        task_id: "task-1".into(),
        title: "Task".into(),
        visible_session_count: 0,
        handoff_copied: false,
    });
    let mut model = opened(data);
    assert_eq!(
        labels(&model),
        ["CONTINUE TASK: Task", "AGENTS", "TOOLS", "Terminal"]
    );
    assert_eq!(
        model.activate(3),
        Some(SessionPickerEvent::Selected(
            SessionPickerOutcome::Continue {
                task_id: "task-1".into(),
                preset: None
            }
        ))
    );
}
