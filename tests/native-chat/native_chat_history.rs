use super::*;
#[path = "native_chat_support.rs"]
mod support;

#[test]
fn native_chat_history_load_and_roundtrip_golden() {
    let text = include_str!("../fixtures/native-chat/history/claude.jsonl");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.jsonl");
    std::fs::write(&path, text).unwrap();
    let entries = load_claude_history(&path).unwrap();
    assert_eq!(entries, parse_claude_transcript_timed(text));
    let restored: Vec<TimedEntry> =
        serde_json::from_value(serde_json::to_value(&entries).unwrap()).unwrap();
    assert_eq!(entries, restored);
    let untimed: Vec<_> = entries.iter().map(|e| e.entry.clone()).collect();
    assert_eq!(untimed, parse_claude_transcript(text));
    support::golden("history/timeline.json", json!(entries));
    support::golden(
        "history/page.json",
        json!(crate::chat_page::history_payloads(restored)),
    );
    let missing = load_claude_history(&dir.path().join("absent.jsonl")).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn native_chat_history_result_cap_is_characters_and_keeps_image_markers() {
    let text = "é".repeat(MAX_RESULT_CHARS + 10);
    let transcript = [
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":text,"is_error":true}]}}),
    ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    let entries = parse_claude_transcript(&transcript);
    let output = entries
        .iter()
        .find_map(|e| match e {
            TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
                output,
                is_error: true,
                ..
            }) => Some(output),
            _ => None,
        })
        .unwrap();
    assert!(output.starts_with(&"é".repeat(MAX_RESULT_CHARS)));
    support::golden(
        "history/result-cap.json",
        json!({"chars":output.chars().count(),"suffix":output.chars().skip(MAX_RESULT_CHARS).collect::<String>()}),
    );
}
