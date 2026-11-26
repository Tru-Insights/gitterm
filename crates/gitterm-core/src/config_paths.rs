//! Shared Iced config path rules and the process-frozen desktop root.
use std::path::{Path, PathBuf};
pub const CONFIG_DIR_NAME: &str = "gitterm-v6";
pub const CONFIG_DIR_ENV: &str = "GITTERM_V6_CONFIG_DIR";
/// Validate a config-dir override read from `env_name`: absolute and
/// non-empty, otherwise ignored with a log line.
pub fn resolve_dir_override(env_name: &str, raw: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let raw = raw?;
    if raw.is_empty() {
        eprintln!("{env_name} is set but empty; ignoring override");
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_relative() {
        eprintln!(
            "{env_name} is relative ({}); ignoring override",
            path.display()
        );
        return None;
    }
    Some(path)
}

pub fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config").join(CONFIG_DIR_NAME)
}

pub fn iced_config_dir(raw: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    resolve_dir_override(CONFIG_DIR_ENV, raw).unwrap_or_else(|| default_config_dir(home))
}

/// Resolve once for every desktop-state consumer in this process, without
/// creating the root. Preview callers may read it but must not lock it.
#[cfg(feature = "runtime")]
pub fn desktop_config_dir() -> std::io::Result<&'static Path> {
    static ROOT: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    match ROOT.get_or_init(|| {
        let home =
            dirs::home_dir().ok_or_else(|| "no home directory for desktop state".to_string())?;
        let path = iced_config_dir(std::env::var_os(CONFIG_DIR_ENV), &home);
        crate::state_lock::resolve_root(&path)
            .map_err(|error| format!("resolve desktop state {}: {error}", path.display()))
    }) {
        Ok(root) => Ok(root.as_path()),
        Err(error) => Err(std::io::Error::other(error.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "runtime")]
    #[test]
    fn frozen_root_child() {
        let Some(other) = std::env::var_os("GITTERM_FROZEN_ROOT_TEST") else {
            return;
        };
        let first = desktop_config_dir().unwrap().to_path_buf();
        let expected = std::fs::canonicalize(std::env::var_os(CONFIG_DIR_ENV).unwrap()).unwrap();
        assert_eq!(first, expected);
        std::env::set_var(CONFIG_DIR_ENV, other);
        assert_eq!(desktop_config_dir().unwrap(), first);
    }
    #[cfg(feature = "runtime")]
    #[test]
    fn resolved_root_is_frozen_for_later_consumers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let input = temp.path().join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &input).unwrap();
        #[cfg(not(unix))]
        let input = root;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config_paths::tests::frozen_root_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CONFIG_DIR_ENV, input)
            .env("GITTERM_FROZEN_ROOT_TEST", temp.path().join("other"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    #[test]
    fn iced_override_and_default_match_host_rules_without_io() {
        let home = Path::new("/fixture/home");
        for raw in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(iced_config_dir(raw, home), home.join(".config/gitterm-v6"));
        }
        assert_eq!(
            iced_config_dir(Some("/fixture/iced".into()), home),
            PathBuf::from("/fixture/iced")
        );
    }
}
