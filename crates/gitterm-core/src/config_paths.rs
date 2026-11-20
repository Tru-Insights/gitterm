//! Shared Iced config path rules; resolving a path performs no filesystem I/O.
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

#[cfg(test)]
mod tests {
    use super::*;
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
