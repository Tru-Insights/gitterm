//! Pure rules of the in-app folder picker (TRU-149), shared by every host.
//! Hosts own the picker state, listings and what "Use this folder" does.

use std::path::{Path, PathBuf};

use crate::chats::ChatIndexEntry;

/// Where the folder picker opens on a machine: the folder last chosen
/// there this session, else `hint`, else the machine's home, else "/".
pub fn start_dir(last_chosen: Option<&str>, hint: Option<&str>, home: Option<&str>) -> String {
    [last_chosen, hint, home]
        .into_iter()
        .flatten()
        .find(|dir| !dir.is_empty())
        .unwrap_or("/")
        .to_string()
}

/// The deepest folder that holds every one of `repos`, if it is below
/// `home` (a repo directly in home, or outside it, gives `None`).
pub fn repos_common_parent_below_home(repos: &[PathBuf], home: &Path) -> Option<PathBuf> {
    let mut parents = repos.iter().filter_map(|repo| repo.parent());
    let mut common: PathBuf = parents.next()?.to_path_buf();
    for parent in parents {
        while !parent.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    (common.starts_with(home) && common != home).then_some(common)
}

/// The picker's "Recent" list for one machine: distinct repo roots from
/// its chat index, most recent first, then its open workspaces' dirs; at
/// most `max`. Chats whose directory is gone are skipped.
pub fn recents(entries: &[ChatIndexEntry], open_dirs: &[PathBuf], max: usize) -> Vec<PathBuf> {
    let mut live: Vec<&ChatIndexEntry> = entries.iter().filter(|entry| !entry.dead_cwd).collect();
    live.sort_by_key(|entry| std::cmp::Reverse(entry.mtime));
    let mut recents: Vec<PathBuf> = Vec::new();
    let candidates = live
        .iter()
        .map(|entry| entry.group_root())
        .chain(open_dirs.iter().map(PathBuf::as_path));
    for dir in candidates {
        if recents.len() >= max {
            break;
        }
        if !recents.iter().any(|seen| seen == dir) {
            recents.push(dir.to_path_buf());
        }
    }
    recents
}

/// Resolve the picker's path field: trimmed, a leading "~" expanded to
/// the machine's `home`, and absolute. The error is shown inline.
pub fn typed_path(input: &str, home: Option<&str>) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Type a folder path".to_string());
    }
    let resolved = if input == "~" || input.starts_with("~/") {
        let Some(home) = home.filter(|home| !home.is_empty()) else {
            return Err(
                "This machine's home folder is not known; type an absolute path".to_string(),
            );
        };
        format!("{}{}", home.trim_end_matches('/'), &input[1..])
    } else {
        input.to_string()
    };
    if !(resolved.starts_with('/') || Path::new(&resolved).is_absolute()) {
        return Err(format!("{resolved} is not an absolute path"));
    }
    Ok(resolved)
}

/// A picker path for display, with the machine's home shown as "~".
pub fn tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|home| home.trim_end_matches('/'))
        .filter(|home| !home.is_empty())
    else {
        return path.to_string();
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_paths_expand_home_and_display_with_tilde() {
        assert_eq!(
            typed_path("~/code", Some("/home/u/")).unwrap(),
            "/home/u/code"
        );
        assert!(typed_path("code", Some("/home/u")).is_err());
        assert_eq!(tilde("/home/u/code", Some("/home/u")), "~/code");
        assert_eq!(start_dir(None, Some(""), Some("/home/u")), "/home/u");
    }
}
