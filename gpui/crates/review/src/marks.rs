//! Viewed marks for the native review: Iced's file read-only, merged with the
//! preview state root's own file. Both use the shared core codec, unchanged.
//!
//! A checkout present in the preview file replaces Iced's marks for that
//! checkout, so clearing a mark Iced set survives a reopen (the codec has no
//! way to say "removed"). Checkouts the preview file lacks follow Iced.
use gitterm_core::review::viewed::ViewedMarks;
use std::{collections::BTreeSet, path::Path};

fn load_dir(dir: Option<&Path>) -> Result<ViewedMarks, String> {
    dir.map_or_else(|| Ok(ViewedMarks::default()), ViewedMarks::load)
}

/// An unreadable file in either root is an error, so nothing unreadable is
/// ever shown as unviewed or overwritten.
pub fn load_merged(iced: Option<&Path>, preview: Option<&Path>) -> Result<ViewedMarks, String> {
    let mut merged = load_dir(iced)?;
    merged.checkouts.extend(load_dir(preview)?.checkouts);
    Ok(merged)
}

/// What the preview file must hold so that `load_merged` returns `merged`:
/// every checkout the preview file already has, plus each one where `merged`
/// differs from Iced's (an emptied checkout is kept, empty).
pub fn overlay(
    iced: Option<&Path>,
    preview: Option<&Path>,
    merged: &ViewedMarks,
) -> Result<ViewedMarks, String> {
    let iced = load_dir(iced)?;
    let previous = load_dir(preview)?;
    let keys: BTreeSet<&String> = iced
        .checkouts
        .keys()
        .chain(merged.checkouts.keys())
        .chain(previous.checkouts.keys())
        .collect();
    let mut out = ViewedMarks::default();
    for key in keys {
        let now = merged.checkouts.get(key).cloned().unwrap_or_default();
        if previous.checkouts.contains_key(key)
            || iced.checkouts.get(key) != merged.checkouts.get(key)
        {
            out.checkouts.insert(key.clone(), now);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::review::viewed::VIEWED_FILE;

    fn write(dir: &Path, json: &str) {
        std::fs::write(dir.join(VIEWED_FILE), json).unwrap();
    }

    #[test]
    fn preview_marks_override_iced_and_absent_files_are_empty() {
        let iced = tempfile::tempdir().unwrap();
        let preview = tempfile::tempdir().unwrap();
        assert_eq!(
            load_merged(Some(iced.path()), Some(preview.path())).unwrap(),
            ViewedMarks::default()
        );
        write(
            iced.path(),
            r#"{"checkouts":{"/c":{"a":"1","b":"2"},"/d":{"x":"9"}}}"#,
        );
        write(preview.path(), r#"{"checkouts":{"/c":{"a":"3","n":"4"}}}"#);
        let merged = load_merged(Some(iced.path()), Some(preview.path())).unwrap();
        assert_eq!(merged.checkouts["/c"]["a"], "3");
        assert!(!merged.checkouts["/c"].contains_key("b"));
        assert_eq!(merged.checkouts["/c"]["n"], "4");
        assert_eq!(merged.checkouts["/d"]["x"], "9");
    }

    #[test]
    fn clearing_an_iced_mark_survives_a_reopen_and_untouched_checkouts_follow_iced() {
        let iced = tempfile::tempdir().unwrap();
        let preview = tempfile::tempdir().unwrap();
        write(
            iced.path(),
            r#"{"checkouts":{"/c":{"a":"1"},"/d":{"x":"9"}}}"#,
        );
        let mut merged = load_merged(Some(iced.path()), Some(preview.path())).unwrap();
        merged.checkouts.remove("/c");
        let out = overlay(Some(iced.path()), Some(preview.path()), &merged).unwrap();
        assert_eq!(out.checkouts.keys().collect::<Vec<_>>(), ["/c"]);
        std::fs::write(preview.path().join(VIEWED_FILE), out.encode().unwrap()).unwrap();
        let reopened = load_merged(Some(iced.path()), Some(preview.path())).unwrap();
        assert!(reopened.checkouts["/c"].is_empty());
        assert_eq!(reopened.checkouts["/d"]["x"], "9");
    }

    #[test]
    fn a_malformed_file_in_either_root_is_an_error() {
        let good = tempfile::tempdir().unwrap();
        let bad = tempfile::tempdir().unwrap();
        write(bad.path(), "not json");
        assert!(load_merged(Some(bad.path()), Some(good.path())).is_err());
        assert!(load_merged(Some(good.path()), Some(bad.path())).is_err());
    }
}
