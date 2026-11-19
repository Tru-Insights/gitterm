use super::*;
use gitterm::{work_documents as docs, workspace_overview as overview};
#[test]
fn work_documents_scan_and_summary_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    let scratch = dir.path().join("scratch");
    for root in [&home, &repo, &scratch] {
        std::fs::create_dir_all(root).unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "root"]);
    let input: Value = serde_json::from_str(&fixture("work/documents.input.json")).unwrap();
    for (name, body) in input.as_object().unwrap() {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body.as_str().unwrap()).unwrap();
    }
    let request = docs::ScanRequest {
        docs_dir: home.join("docs"),
        written: vec![repo.join("docs/ordinary.md"), scratch.join("private.md")],
        repos: vec![repo.clone()],
        task_worktrees: vec![scratch],
    };
    let before = git(&repo, &["status", "--porcelain"]);
    let scanned = docs::scan(&request);
    assert_eq!(git(&repo, &["status", "--porcelain"]), before);
    let mut rows=scanned.documents.iter().map(|d|json!({"rel":d.rel,"title":d.title,"copies":d.copies.len(),"differ":d.differ,"source":d.newest().source.label()})).collect::<Vec<_>>();
    rows.sort_by_key(|r| r["rel"].as_str().unwrap().to_owned());
    let mut root = super::app::chat(
        "fixture-root",
        chats::ChatBackend::Claude,
        "/fixture/work",
        "Synthetic root",
    );
    root.facts=Some(serde_json::from_value(json!({"issue_keys":["TRU-999"],"children":[{"session_id":"fixture-child","kind":"worker","title":"Synthetic child"}],"repos_touched":[{"dir":"/fixture/repo","edits":2}]})).unwrap());
    let mut child = super::app::chat(
        "fixture-child",
        chats::ChatBackend::Codex,
        "/fixture/repo",
        "Synthetic child",
    );
    child.root_id = Some("fixture-root".into());
    let entries = [root, child];
    let summary = overview::WorkSummary::build(
        &entries,
        Path::new("/fixture/work"),
        &[],
        &overview::Focus::Latest,
        std::time::UNIX_EPOCH,
    );
    golden(
        "work/read.trace.json",
        json!({"documents":rows,"truncated":scanned.truncated,
        "summary":{"chat_count":summary.chat_count,"has_facts":summary.has_facts,"focused":summary.focused,"issue_keys":summary.issue_keys,
        "repos":summary.repos.iter().map(|r|json!({"dir":r.dir,"edits":r.edits})).collect::<Vec<_>>()}}),
    );
}
