//! Codex trust decisions over fixed fixtures. The root workspace runs this
//! file with its pinned toml 0.8.2; the GPUI shell includes the same file
//! (`gpui/crates/shell/tests/codex_trust.rs`) under GPUI's resolved toml 0.8.x.
//! The expected outcomes are exact, so both passing means identical results.
//! Mirrors the trust tests in `workers.rs` (`reliability_tests`).
use gitterm_core::workers::{
    claude_task_trust, codex_task_trust, codex_trust_value, trust_main_repository,
    validate_codex_trust_command, validate_codex_trust_home, with_codex_trust,
};
use std::path::{Path, PathBuf};

fn repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    git2::Repository::init(dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn committed(dir: &Path) -> (PathBuf, git2::Repository) {
    let root = repo(dir);
    let repository = git2::Repository::open(&root).unwrap();
    {
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
    }
    (root, repository)
}

fn linked_repo(temp: &Path) -> (PathBuf, PathBuf) {
    let (root, repository) = committed(&temp.join("main.repo space ' $quote"));
    let linked = temp.join("linked worker");
    repository.worktree("worker", &linked, None).unwrap();
    (root, linked)
}

fn key(path: &Path) -> String {
    serde_json::to_string(path.to_str().unwrap()).unwrap()
}

#[test]
fn workspace_trust_is_run_local_and_rechecks_explicit_denials() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(&temp.path().join("sample.repo"));
    let outside = repo(&temp.path().join("outside"));
    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        "approval_policy='on-request'\nsandbox_mode='workspace-write'\n",
    )
    .unwrap();
    let original = std::fs::read(&config).unwrap();
    let value = codex_task_trust(
        "codex -m example-model",
        &root,
        std::slice::from_ref(&root),
        &config,
    )
    .unwrap()
    .unwrap();
    let parsed: toml::Value = toml::from_str(&value).unwrap();
    assert_eq!(
        parsed["projects"][root.to_str().unwrap()]["trust_level"].as_str(),
        Some("trusted")
    );
    assert_eq!(std::fs::read(&config).unwrap(), original);
    assert!(
        codex_task_trust("codex", &outside, std::slice::from_ref(&root), &config)
            .unwrap()
            .is_none()
    );
    assert!(
        codex_task_trust("codex", &root, &[temp.path().to_path_buf()], &config)
            .unwrap()
            .is_none()
    );
    std::fs::write(
        &config,
        format!("[projects.{}]\ntrust_level='untrusted'\n", key(&root)),
    )
    .unwrap();
    let denied = std::fs::read(&config).unwrap();
    let error = codex_task_trust("codex", &root, std::slice::from_ref(&root), &config).unwrap_err();
    assert!(error.contains("explicitly untrusted"), "{error}");
    assert!(error.contains("Change its trust_level"), "{error}");
    assert_eq!(std::fs::read(&config).unwrap(), denied);
    assert!(!codex_trust_value(&root)
        .unwrap()
        .contains("approval_policy"));
}

#[test]
fn worktree_trust_resolves_main_and_denies_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let (root, repository) = committed(&temp.path().join("main"));
    let checkout = temp.path().join("linked");
    repository.worktree("worker", &checkout, None).unwrap();
    assert_eq!(trust_main_repository(&checkout).unwrap(), root);
    let config = temp.path().join("config.toml");
    assert_eq!(
        codex_task_trust("codex", &checkout, std::slice::from_ref(&root), &config).unwrap(),
        Some(codex_trust_value(&root).unwrap())
    );
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    std::fs::write(
        &config,
        format!("[projects.{}]\ntrust_level='untrusted'\n", key(&alias)),
    )
    .unwrap();
    assert!(codex_task_trust("codex", &checkout, &[root], &config)
        .unwrap_err()
        .contains("explicitly untrusted"));
}

#[test]
fn forged_worktree_pointer_without_reciprocal_registration_gets_no_trust() {
    let temp = tempfile::tempdir().unwrap();
    let (root, linked) = linked_repo(temp.path());
    let forged = temp.path().join("forged");
    std::fs::create_dir(&forged).unwrap();
    std::fs::copy(linked.join(".git"), forged.join(".git")).unwrap();
    assert!(trust_main_repository(&forged)
        .unwrap_err()
        .contains("registration"));
    let config = temp.path().join("codex.toml");
    assert!(codex_task_trust("codex", &forged, std::slice::from_ref(&root), &config).is_err());
    let claude_config = temp.path().join("claude.json");
    std::fs::write(&claude_config, "{}").unwrap();
    assert!(claude_task_trust(&forged, std::slice::from_ref(&root), &claude_config).is_err());
    let original_config = std::fs::read(&claude_config).unwrap();
    let symlinked = temp.path().join("symlinked-forgery");
    std::fs::create_dir(&symlinked).unwrap();
    std::os::unix::fs::symlink(linked.join(".git"), symlinked.join(".git")).unwrap();
    assert!(trust_main_repository(&symlinked).is_err());
    assert!(codex_task_trust("codex", &symlinked, std::slice::from_ref(&root), &config).is_err());
    assert!(claude_task_trust(&symlinked, std::slice::from_ref(&root), &claude_config).is_err());
    assert_eq!(std::fs::read(&claude_config).unwrap(), original_config);
}

#[test]
fn malformed_trust_and_redirected_presets_fail_loudly() {
    for command in [
        "codex -C /elsewhere",
        "codex --cd=/elsewhere",
        "codex -C/elsewhere",
        "codex -p other",
        "codex --profile=other",
        "codex -c 'projects={}'",
        "codex --config='projects={}'",
        "codex -cprojects={}",
        "codex -c=projects={}",
        "codex -c=profile=other",
        "codex --no-alt-screen;codex",
        "codex $(printf -- -C/elsewhere)",
        "codex --no-alt-screen&&codex",
        "codex --config 'profile=other'",
    ] {
        assert!(validate_codex_trust_command(command).is_err(), "{command}");
    }
    let temp = tempfile::tempdir().unwrap();
    let root = repo(&temp.path().join("repo"));
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "[projects\n").unwrap();
    assert!(codex_task_trust("codex", &root, &[], &config)
        .unwrap_err()
        .contains("parse Codex trust list"));
    for name in ["HOME", "CODEX_HOME"] {
        let mut env = vec![
            ("HOME".into(), temp.path().display().to_string()),
            (
                "CODEX_HOME".into(),
                temp.path().join(".codex").display().to_string(),
            ),
        ];
        env.push((name.into(), "/alternate-source".into()));
        assert!(validate_codex_trust_home(temp.path(), &env, |_| None)
            .unwrap_err()
            .contains(name));
    }
}

#[test]
fn malformed_private_config_does_not_expose_source_text() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(&temp.path().join("repo"));
    let config = temp.path().join("config.toml");
    let secret = "SYNTHETIC_SECRET_MUST_NOT_APPEAR";
    std::fs::write(&config, format!("api_key = \"{secret}\n")).unwrap();
    let error = codex_task_trust("codex", &root, std::slice::from_ref(&root), &config).unwrap_err();
    assert!(error.contains("invalid TOML"));
    assert!(!error.contains(secret), "private config source leaked");
}

#[test]
fn inline_path_and_shell_quoting_preserve_special_characters() {
    let path = Path::new("/tmp/sample.repo space \"quote\" ' $value");
    let value = codex_trust_value(path).unwrap();
    let parsed: toml::Value = toml::from_str(&value).unwrap();
    assert_eq!(
        parsed["projects"][path.to_str().unwrap()]["trust_level"].as_str(),
        Some("trusted")
    );
    let command = with_codex_trust("codex", &value).unwrap();
    assert_eq!(
        shell_words::split(&command).unwrap(),
        vec!["codex", "-c", &value]
    );
}

/// Exact outcomes, including TOML error byte offsets, for config shapes the
/// parser could treat differently between patch versions.
#[test]
fn trust_matrix_outcomes_are_exact() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let (root, repository) = committed(&base.join("main"));
    let checkout = base.join("linked");
    repository.worktree("worker", &checkout, None).unwrap();
    let outside = repo(&base.join("outside"));
    let config = base.join("config.toml");
    let r = key(&root);
    let o = key(&outside);
    let b = key(&base);
    let escaped = r.replacen('/', "\\u002F", 1);
    let literal = format!("'{}'", root.to_str().unwrap());
    let untrusted = "err: Refusing Codex worker: repository <tmp>/main is explicitly untrusted in <tmp>/config.toml. Change its trust_level there before launching";
    let invalid_level = "err: invalid project trust_level in <tmp>/config.toml";
    let not_table = "err: projects in <tmp>/config.toml must be a table";
    let duplicate = format!(
        "[projects.{r}]\ntrust_level=\"trusted\"\n[projects.{r}]\ntrust_level=\"untrusted\"\n"
    );
    // The second header, wherever the temp path puts it.
    let duplicate_error = format!(
        "err: parse Codex trust list <tmp>/config.toml: invalid TOML at byte {}",
        duplicate.rfind("[projects.").unwrap()
    );
    let cases: Vec<(&str, Option<Vec<u8>>, &str)> = vec![
        ("missing file", None, "trusted"),
        ("empty", Some(b"".to_vec()), "trusted"),
        ("whitespace only", Some(b" \n\t\n".to_vec()), "trusted"),
        (
            "unrelated settings",
            Some(b"approval_policy='on-request'\n[tui]\nx=1\n".to_vec()),
            "trusted",
        ),
        ("empty projects", Some(b"[projects]\n".to_vec()), "trusted"),
        (
            "explicitly trusted",
            Some(format!("[projects.{r}]\ntrust_level=\"trusted\"\n").into_bytes()),
            "trusted",
        ),
        (
            "entry without trust_level",
            Some(format!("[projects.{r}]\nnote=1\n").into_bytes()),
            "trusted",
        ),
        (
            "untrusted table",
            Some(format!("[projects.{r}]\ntrust_level=\"untrusted\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted inline",
            Some(format!("projects = {{ {r} = {{ trust_level = 'untrusted' }} }}\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted dotted key",
            Some(format!("projects.{r}.trust_level = \"untrusted\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted literal-string key",
            Some(format!("[projects.{literal}]\ntrust_level=\"untrusted\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted escaped key",
            Some(format!("[projects.{escaped}]\ntrust_level=\"untrusted\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted multi-line string",
            Some(format!("[projects.{r}]\ntrust_level=\"\"\"untrusted\"\"\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted with comment, CRLF, no final newline",
            Some(format!("# c\r\n[projects.{r}]\r\ntrust_level = \"untrusted\" # why").into_bytes()),
            untrusted,
        ),
        (
            "untrusted ancestor",
            Some(format!("[projects.{b}]\ntrust_level=\"untrusted\"\n").into_bytes()),
            untrusted,
        ),
        (
            "untrusted other repo",
            Some(format!("[projects.{o}]\ntrust_level=\"untrusted\"\n").into_bytes()),
            "trusted",
        ),
        (
            "trusted other then untrusted root",
            Some(
                format!(
                    "[projects.{o}]\ntrust_level=\"trusted\"\n[projects.{r}]\ntrust_level=\"untrusted\"\n"
                )
                .into_bytes(),
            ),
            untrusted,
        ),
        (
            "unknown trust_level",
            Some(format!("[projects.{r}]\ntrust_level=\"maybe\"\n").into_bytes()),
            invalid_level,
        ),
        (
            "numeric trust_level",
            Some(format!("[projects.{r}]\ntrust_level=1\n").into_bytes()),
            invalid_level,
        ),
        ("projects scalar", Some(b"projects = 1\n".to_vec()), not_table),
        ("projects array", Some(b"projects = []\n".to_vec()), not_table),
        (
            "unclosed table header",
            Some(b"[projects\n".to_vec()),
            "err: parse Codex trust list <tmp>/config.toml: invalid TOML at byte 9",
        ),
        (
            "unterminated string",
            Some(b"api_key = \"secret\n".to_vec()),
            "err: parse Codex trust list <tmp>/config.toml: invalid TOML at byte 17",
        ),
        (
            "duplicate key",
            Some(b"a=1\na=2\n".to_vec()),
            "err: parse Codex trust list <tmp>/config.toml: invalid TOML at byte 4",
        ),
        (
            "duplicate table",
            Some(duplicate.clone().into_bytes()),
            &duplicate_error,
        ),
        (
            "byte order mark",
            Some(b"\xEF\xBB\xBFapproval_policy='on-request'\n".to_vec()),
            "trusted",
        ),
    ];
    let value = codex_trust_value(&root).unwrap();
    let normalize = |text: String| text.replace(base.to_str().unwrap(), "<tmp>");
    let mut failures = Vec::new();
    for cwd in [&root, &checkout] {
        for (name, text, expected) in &cases {
            let _ = std::fs::remove_file(&config);
            if let Some(text) = text {
                std::fs::write(&config, text).unwrap();
            }
            let actual = match codex_task_trust("codex", cwd, std::slice::from_ref(&root), &config)
            {
                Ok(None) => "none".to_string(),
                Ok(Some(found)) if found == value => "trusted".to_string(),
                Ok(Some(found)) => format!("value {}", normalize(found)),
                Err(error) => format!("err: {}", normalize(error)),
            };
            if actual != *expected {
                failures.push(format!(
                    "{name} (cwd {}): {actual}",
                    normalize(cwd.display().to_string())
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
