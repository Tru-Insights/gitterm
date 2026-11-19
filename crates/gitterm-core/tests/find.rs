//! Synthetic fixtures shared with the unchanged Iced stage-4/5 baseline.
use gitterm_core::chats::{ChatBackend, ChatIndexEntry, ChatPreview};
use gitterm_core::find::{filter_chat_entries, load_preview, load_snippet_preview};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[test]
fn core_filters_match_the_existing_iced_trace() {
    let input: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/stage45/work/find.input.json"
    ))
    .unwrap();
    let entries = input
        .as_array()
        .unwrap()
        .iter()
        .map(|row| ChatIndexEntry {
            id: row["id"].as_str().unwrap().into(),
            backend: ChatBackend::from_label(row["backend"].as_str().unwrap()).unwrap(),
            path: PathBuf::new(),
            cwd: row["cwd"].as_str().unwrap().into(),
            repo_root: row["repo_root"].as_str().map(PathBuf::from),
            is_worktree: false,
            branch: None,
            title: row["title"].as_str().unwrap().into(),
            latest: None,
            mtime: UNIX_EPOCH,
            size: 0,
            dead_cwd: true,
            facts: row
                .get("facts")
                .cloned()
                .map(|f| serde_json::from_value(f).unwrap()),
            root_id: None,
        })
        .collect::<Vec<_>>();
    let trace: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/stage45/work/find.trace.json"
    ))
    .unwrap();
    for (i, (root, backend, query)) in [
        (Some(Path::new("/fixture/alpha")), None, ""),
        (None, None, ""),
        (None, Some(ChatBackend::Codex), "synthetic"),
        (Some(Path::new("/fixture/alpha")), None, "tru-999"),
    ]
    .into_iter()
    .enumerate()
    {
        let actual = filter_chat_entries(&entries, root, backend, query);
        assert_eq!(
            json!(actual
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()),
            trace[i]["ids"]
        );
    }
    assert!(filter_chat_entries(&entries, None, Some(ChatBackend::Pi), "").is_empty());
    assert!(filter_chat_entries(&entries, None, None, "missing").is_empty());
}

#[test]
fn previews_work_without_runtime_and_leave_synthetic_sources_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    for (backend, body) in [
        (
            ChatBackend::Claude,
            include_str!("../../../tests/fixtures/chats/claude.jsonl"),
        ),
        (
            ChatBackend::Codex,
            include_str!("../../../tests/fixtures/chats/codex.jsonl"),
        ),
        (
            ChatBackend::Pi,
            include_str!("../../../tests/fixtures/chats/pi.jsonl"),
        ),
    ] {
        let path = temp.path().join(format!("{}.jsonl", backend.label()));
        std::fs::write(&path, body).unwrap();
        let preview = load_preview(&path, backend);
        assert!(!preview.messages.is_empty());
        assert_eq!(load_snippet_preview(&path, backend).unwrap(), preview);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(
            load_preview(&temp.path().join("missing"), backend),
            ChatPreview::default()
        );
        assert_eq!(
            load_snippet_preview(&temp.path().join("missing"), backend)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
