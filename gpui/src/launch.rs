//! Only explicit routes and folder inputs; no implicit demo or V6 restore.
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    Shell { folders: Vec<PathBuf>, sample: bool },
    Terminal,
    Chat(PathBuf),
}
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Option<Launch> {
    let args: Vec<_> = args.into_iter().collect();
    if args.len() == 1 && args[0] == "--terminal" {
        return Some(Launch::Terminal);
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
            route(&["--chat-live", "fixture.jsonl"]),
            Some(Launch::Chat("fixture.jsonl".into()))
        );
    }
}
