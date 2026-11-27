use gitterm_core::state_lock::{StateLock, LOCK_FILE};
use std::process::Command;

#[test]
fn busy_state_root_exits_with_visible_error_without_changing_root() {
    let temp = tempfile::tempdir().unwrap();
    let input_root = temp.path().join("isolated-state");
    std::fs::create_dir(&input_root).unwrap();
    let root = std::fs::canonicalize(input_root).unwrap();
    let lock = StateLock::acquire(&root).unwrap();
    let before: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect();

    let output = Command::new(env!("CARGO_BIN_EXE_gitterm-v6"))
        .env("GITTERM_V6_CONFIG_DIR", &root)
        .env("GITTERM_V6_NO_DIALOG", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "GitTerm state root is already in use: {}",
            root.display()
        )),
        "unexpected stderr: {stderr}"
    );
    let after: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(before, after);
    assert!(root.join(LOCK_FILE).exists());
    drop(lock);
}
