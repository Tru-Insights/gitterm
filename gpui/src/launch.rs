//! Only explicit routes and folder inputs; no implicit demo or V6 restore.
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    Shell { folders: Vec<PathBuf>, sample: bool },
    Terminal,
    Chat(PathBuf),
    Review(Vec<OsString>),
    ReviewRepo(PathBuf),
}
impl Launch {
    /// Compose C2 roots lazily: legacy terminal/repo review and shell admission
    /// must not depend on provider overrides or preview metadata validation.
    pub fn authorize(
        &self,
        compose: impl FnOnce() -> std::io::Result<std::sync::Arc<gitterm_core::preview::Preview>>,
    ) -> std::io::Result<()> {
        if matches!(
            self,
            Self::Terminal | Self::ReviewRepo(_) | Self::Review(_) | Self::Shell { .. }
        ) {
            return Ok(());
        }
        let preview = compose()?;
        let source = |path: &std::path::Path| -> std::io::Result<PathBuf> {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            preview.protected().check(&path)
        };
        match self {
            Self::Chat(path) => {
                source(path)?;
            }
            _ => unreachable!("legacy routes returned above"),
        }
        Ok(())
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Option<Launch> {
    let args: Vec<_> = args.into_iter().collect();
    if args.len() == 1 && args[0] == "--terminal" {
        return Some(Launch::Terminal);
    }
    if args.first().is_some_and(|arg| arg == "--review-repo") {
        return (args.len() == 2 && !args[1].to_string_lossy().starts_with("--"))
            .then(|| Launch::ReviewRepo((&args[1]).into()));
    }
    if args.first().is_some_and(|arg| arg == "--review") {
        return Some(Launch::Review(args[1..].to_vec()));
    }
    if args.len() == 2 && matches!(args[0].to_str(), Some("--chat" | "--chat-live")) {
        return Some(Launch::Chat((&args[1]).into()));
    }
    let mut folders = vec![];
    let mut sample = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--sample") if !sample => sample = true,
            Some("--workspace") => {
                let path = args
                    .next()
                    .filter(|path| !path.to_string_lossy().starts_with("--"))?;
                folders.push(path.into());
            }
            _ => return None,
        }
    }
    if sample && !folders.is_empty() {
        return None;
    }
    Some(Launch::Shell { folders, sample })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn route(args: &[&str]) -> Option<Launch> {
        parse(args.iter().map(OsString::from))
    }
    #[test]
    fn repo_review_requires_exactly_one_directory() {
        assert_eq!(
            route(&["--review-repo", "/fixture/repo"]),
            Some(Launch::ReviewRepo("/fixture/repo".into()))
        );
        for args in [
            vec!["--review-repo"],
            vec!["--review-repo", "--sample"],
            vec!["--review-repo", "/repo", "extra"],
        ] {
            assert_eq!(route(&args), None);
        }
    }
    #[test]
    fn explicit_folders_repeat_and_sample_is_opt_in_and_exclusive() {
        assert_eq!(
            route(&[]),
            Some(Launch::Shell {
                folders: vec![],
                sample: false
            })
        );
        assert_eq!(
            route(&[
                "--workspace",
                "/fixture/alpha",
                "--workspace",
                "/fixture/beta"
            ]),
            Some(Launch::Shell {
                folders: vec!["/fixture/alpha".into(), "/fixture/beta".into()],
                sample: false
            })
        );
        assert_eq!(
            route(&["--sample"]),
            Some(Launch::Shell {
                folders: vec![],
                sample: true
            })
        );
        for invalid in [
            vec!["--sample", "--workspace", "/fixture"],
            vec!["--workspace"],
            vec!["--workspace", "--sample"],
            vec!["--terminal", "extra"],
            vec!["--sample", "--sample"],
            vec!["--unknown"],
        ] {
            assert_eq!(route(&invalid), None);
        }
        assert_eq!(route(&["--terminal"]), Some(Launch::Terminal));
        assert_eq!(
            route(&["--review", "fixtures/showcase"]),
            Some(Launch::Review(vec!["fixtures/showcase".into()]))
        );
        assert_eq!(
            route(&["--chat-live", "fixture.jsonl"]),
            Some(Launch::Chat("fixture.jsonl".into()))
        );
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn legacy_terminal_review_and_shell_routes_are_not_blocked() {
        let dir = std::env::temp_dir().join(format!("gpui-route-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let protected = gitterm_core::preview::ProtectedRoots::new(&dir.join("host"), []).unwrap();
        let preview = std::sync::Arc::new(
            gitterm_core::preview::Preview::validate(&dir.join("preview"), protected).unwrap(),
        );
        let forbidden = dir.join("host/.config/gitterm-v6/sentinel");
        for args in [
            vec!["--chat".into(), forbidden.clone().into_os_string()],
            vec!["--chat-live".into(), forbidden.clone().into_os_string()],
        ] {
            let route = parse(args).unwrap();
            let error = route
                .authorize(|| Ok(preview.clone()))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("standalone-terminal")
                    || error.contains(forbidden.to_str().unwrap()),
                "{error}"
            );
        }
        for args in [
            vec![],
            vec!["--sample".into()],
            vec!["--terminal".into()],
            vec![
                "--review".into(),
                dir.join("fixture-review").into_os_string(),
            ],
            vec![
                "--review-repo".into(),
                dir.join("missing-repo").into_os_string(),
            ],
            vec![
                "--workspace".into(),
                dir.join("missing-workspace").into_os_string(),
            ],
        ] {
            assert!(
                parse(args)
                    .unwrap()
                    .authorize(|| panic!("legacy route composed preview roots"))
                    .is_ok()
            );
        }
        let fixture = dir.join("fixture.jsonl");
        assert!(
            Launch::Chat(fixture)
                .authorize(|| Ok(preview.clone()))
                .is_ok()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
