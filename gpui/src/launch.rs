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
    /// All routes share the closed capability boundary before their first effect.
    pub fn authorize(&self, preview: &gitterm_core::preview::Preview) -> std::io::Result<()> {
        use gitterm_core::preview::Effect;
        let source = |path: &std::path::Path| -> std::io::Result<PathBuf> {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            preview.protected().check(&path)
        };
        match self {
            Self::Terminal => preview.capabilities().authorize(
                Effect::Terminal,
                std::path::Path::new("<standalone-terminal>"),
            ),
            Self::ReviewRepo(path) => preview.capabilities().authorize(Effect::Git, path),
            Self::Chat(path) => {
                source(path)?;
                Ok(())
            }
            Self::Review(args) => {
                for arg in args {
                    let root = source(std::path::Path::new(arg))?;
                    preview.protected().check(&root.join("review.json"))?;
                }
                Ok(())
            }
            Self::Shell { folders, .. } => {
                for path in folders {
                    source(path)?;
                    gitterm_core::preview::inspect_git(path, preview.protected())?;
                }
                Ok(())
            }
        }
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
    fn every_route_checks_attempted_targets_without_executable_effects() {
        let dir = std::env::temp_dir().join(format!("gpui-route-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let protected = gitterm_core::preview::ProtectedRoots::new(&dir.join("host"), []).unwrap();
        let preview =
            gitterm_core::preview::Preview::validate(&dir.join("preview"), protected).unwrap();
        let forbidden = dir.join("host/.config/gitterm-v6/sentinel");
        for args in [
            vec!["--terminal".into()],
            vec!["--review-repo".into(), forbidden.clone().into_os_string()],
            vec!["--chat".into(), forbidden.clone().into_os_string()],
            vec!["--chat-live".into(), forbidden.clone().into_os_string()],
            vec!["--review".into(), forbidden.clone().into_os_string()],
            vec!["--workspace".into(), forbidden.clone().into_os_string()],
        ] {
            let route = parse(args).unwrap();
            let error = route.authorize(&preview).unwrap_err().to_string();
            assert!(
                error.contains("standalone-terminal")
                    || error.contains(forbidden.to_str().unwrap()),
                "{error}"
            );
        }
        for args in [vec![], vec!["--sample".into()]] {
            assert!(parse(args).unwrap().authorize(&preview).is_ok());
        }
        let fixture = dir.join("fixture.jsonl");
        assert!(Launch::Chat(fixture).authorize(&preview).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
