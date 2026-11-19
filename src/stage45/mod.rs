//! TRU-274 PR 0: fixtures observed through the pre-extraction Iced paths.
use super::*;
use serde_json::{json, Value};

mod app;
mod review;
mod sessions;
mod stores;
mod usage;
mod work;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stage45");
fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap()
}
fn golden(name: &str, actual: Value) {
    let expected: Value = serde_json::from_str(&fixture(name)).unwrap();
    assert_eq!(actual, expected, "fixture: {name}");
}
// Config and snapshot roots are process globals. Set them before the child
// starts, never mutate the parent environment while other tests are running.
fn isolated(name: &str, module: &str) -> bool {
    if std::env::var("GITTERM_V6_STAGE45_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("baseline_tests::stage45::{module}::{name}"),
            "--nocapture",
        ])
        .env(config::CONFIG_DIR_ENV, dir.path())
        .env("GITTERM_V6_STAGE45_CHILD", name)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    true
}
fn git(dir: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        command.env_remove(key);
    }
    let out = command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
