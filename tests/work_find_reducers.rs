//! The new Find reader consumes the same baseline inputs without rewriting
//! Iced's recorded traces (including Iced's historical late-preview caching).
use gitterm_core::chats::{ChatBackend, ChatIndexEntry, ChatScope};
use gitterm_core::find::state::{Effect, Filter, Intent, State};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

#[test]
fn find_reducer_preserves_stage45_filter_fixture() {
    let input: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/stage45/work/find.input.json")).unwrap();
    let entries = input
        .iter()
        .map(|row| ChatIndexEntry {
            id: row["id"].as_str().unwrap().into(),
            backend: ChatBackend::from_label(row["backend"].as_str().unwrap()).unwrap(),
            path: PathBuf::from("/fixture/transcript.jsonl"),
            cwd: PathBuf::from(row["cwd"].as_str().unwrap()),
            repo_root: row["repo_root"].as_str().map(PathBuf::from),
            is_worktree: false,
            branch: None,
            title: row["title"].as_str().unwrap().into(),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            dead_cwd: false,
            facts: row
                .get("facts")
                .map(|facts| serde_json::from_value(facts.clone()).unwrap()),
            root_id: None,
        })
        .collect();
    let mut state = State::<gitterm_core::chats::ChatPreview>::default();
    let now = Instant::now();
    let effects = state.reduce(Intent::LoadIndex {
        machine: None,
        now,
        force: false,
    });
    let Effect::LoadIndex(stamp) = &effects[0] else {
        panic!()
    };
    state.reduce(Intent::IndexFinished {
        stamp: stamp.clone(),
        result: Ok(entries),
        now,
    });
    let expected: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/stage45/work/find.trace.json")).unwrap();
    for row in expected.iter().take(4) {
        state.reduce(Intent::SetFilter(Filter {
            query: row["query"].as_str().unwrap().into(),
            scope: match row["scope"].as_str().unwrap() {
                "Workspace" => ChatScope::Workspace,
                "Machine" => ChatScope::Machine,
                "Everywhere" => ChatScope::Everywhere,
                _ => panic!("unknown fixture scope"),
            },
            backend: row["backend"]
                .as_str()
                .map(|label| ChatBackend::from_label(label).unwrap()),
            machine: None,
            workspace_root: Some(PathBuf::from("/fixture/alpha")),
        }));
        let ids: Vec<_> = state
            .filtered_entries()
            .iter()
            .map(|(_, entry)| entry.id.as_str())
            .collect();
        let expected: Vec<_> = row["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        assert_eq!(ids, expected);
    }
}
