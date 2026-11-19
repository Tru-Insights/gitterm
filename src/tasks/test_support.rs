fn sample_task(task_id: &str) -> TaskRecord {
        TaskRecord::new_draft(
            NewTaskRecord {
                task_id: task_id.to_string(),
                title: "Add durable tasks".to_string(),
                objective: "Persist task state without opening a workspace".to_string(),
                workspace: WorkspaceIdentity {
                    name: "GitTerm V6".to_string(),
                    location: WorkspaceLocationIdentity::Local {
                        directory: PathBuf::from("/repo with spaces/gitterm-v6"),
                    },
                },
                repository: RepositoryIdentity {
                    common_dir: PathBuf::from("/repo with spaces/gitterm-v6/.git"),
                    remote_url: Some("https://github.com/Tru-Insights/gitterm.git".to_string()),
                },
                issue: Some(IssueReference {
                    provider: IssueProvider::Linear,
                    key: "TRU-104".to_string(),
                    url: Some("https://linear.app/example/TRU-104".to_string()),
                }),
                base: GitBase {
                    reference: "v6".to_string(),
                    commit: "0123456789abcdef".to_string(),
                },
                branch: format!("task/{task_id}-durable-tasks"),
                executor: ExecutorTarget::Local,
                harness: Some(HarnessSelection {
                    kind: HarnessKind::TerminalPreset {
                        preset_name: "Codex".to_string(),
                    },
                    model: Some("gpt-5.6".to_string()),
                }),
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            },
            "2026-08-19T08:00:00Z",
        )
    }
