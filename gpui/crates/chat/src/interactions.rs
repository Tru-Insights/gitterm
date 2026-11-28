//! Transcript gestures and Iced's bounded edit presentation.
use crate::model::EditDiff;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptAction {
    SendDelegation {
        id: String,
        finding_ids: Vec<String>,
    },
    RerunDelegation {
        id: String,
    },
    OpenFile {
        path: PathBuf,
        review: bool,
        line: Option<usize>,
    },
    OpenWorker {
        id: String,
    },
    PreviewOnly {
        label: String,
    },
    /// A delegation card's Dismiss, Cancel or Stop tracking.
    Dismiss(DismissRequest),
    Withdraw {
        message_id: String,
    },
}

/// Iced `delegation_dismiss` for one card: `active` is the delegation's
/// `status.is_active()`, which makes the dismissal a cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DismissRequest {
    pub id: String,
    pub label: String,
    pub active: bool,
}
/// The host's answer to a [`DismissRequest`]: the notice to show. Without a
/// gate the view shows the preview notice itself.
#[cfg(target_os = "macos")]
pub type DismissGate = std::rc::Rc<dyn Fn(&DismissRequest, &gpui::App) -> String>;

pub fn preview_notice(label: &str) -> String {
    format!("{label} · not available in preview")
}

/// Iced intercepts click, not auxclick, and only forwards HTTP(S) destinations.
pub fn http_link(url: &str) -> bool {
    url.get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case("http:"))
        || url
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("https:"))
}

pub fn history_start(rows: usize) -> usize {
    rows.saturating_sub(150) / 50 * 50
}

/// Iced's `foldSegment` tail: after a fold the blocks hidden in detached
/// 50-block chunks (oldest first, any remainder newest) come back, newest
/// first, for as long as whole chunks fit in the 150 the document keeps.
/// Returns how many blocks to restore.
pub fn blocks_restored_by_fold(hidden: usize, live: usize) -> usize {
    let mut room = 150usize.saturating_sub(live);
    let mut sizes = vec![50; hidden / 50];
    if !hidden.is_multiple_of(50) {
        sizes.push(hidden % 50);
    }
    let mut restored = 0;
    for size in sizes.into_iter().rev() {
        if size > room {
            break;
        }
        room -= size;
        restored += size;
    }
    restored
}

pub fn earlier_label(hidden: usize) -> String {
    format!(
        "Show earlier · {hidden} block{} hidden",
        if hidden == 1 { "" } else { "s" }
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditLine {
    pub sign: char,
    pub text: String,
}
#[derive(Debug, Clone)]
pub struct EditLines {
    pub rows: Vec<EditLine>,
    pub skipped_before: usize,
    pub skipped_after: usize,
    pub capped: bool,
}
impl EditLines {
    pub fn shown(&self, expanded: bool) -> &[EditLine] {
        &self.rows[..if expanded {
            self.rows.len()
        } else {
            self.rows.len().min(40)
        }]
    }
    pub fn more_label(&self, expanded: bool) -> Option<String> {
        (!expanded && self.rows.len() > 40).then(|| format!("Show all {} lines", self.rows.len()))
    }
}

/// Same prefix/suffix context, LCS tie-break and cap as agent_chat.html::lineDiff.
pub fn edit_lines(edit: &EditDiff) -> EditLines {
    fn lines(text: &str) -> Vec<&str> {
        if text.is_empty() {
            vec![]
        } else {
            text.strip_suffix('\n')
                .unwrap_or(text)
                .split('\n')
                .collect()
        }
    }
    let a = lines(&edit.old);
    let b = lines(&edit.new);
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let skipped_before = prefix.saturating_sub(3);
    let mut rows: Vec<_> = a[skipped_before..prefix]
        .iter()
        .map(|text| EditLine {
            sign: ' ',
            text: (*text).into(),
        })
        .collect();
    let a = &a[prefix..a.len() - suffix];
    let b = &b[prefix..b.len() - suffix];
    let capped = a.len() > 400 || b.len() > 400;
    if capped || a.is_empty() || b.is_empty() {
        rows.extend(a.iter().map(|text| EditLine {
            sign: '-',
            text: (*text).into(),
        }));
        rows.extend(b.iter().map(|text| EditLine {
            sign: '+',
            text: (*text).into(),
        }));
    } else {
        let width = b.len() + 1;
        let mut lcs = vec![0u16; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * width + j] = if a[i] == b[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            let (sign, text) = if i < a.len() && j < b.len() && a[i] == b[j] {
                let text = a[i];
                i += 1;
                j += 1;
                (' ', text)
            } else if j >= b.len()
                || (i < a.len() && lcs[(i + 1) * width + j] >= lcs[i * width + j + 1])
            {
                let text = a[i];
                i += 1;
                ('-', text)
            } else {
                let text = b[j];
                j += 1;
                ('+', text)
            };
            rows.push(EditLine {
                sign,
                text: text.into(),
            });
        }
    }
    let all_a = lines(&edit.old);
    rows.extend(
        all_a[all_a.len() - suffix..]
            .iter()
            .take(3)
            .map(|text| EditLine {
                sign: ' ',
                text: (*text).into(),
            }),
    );
    EditLines {
        rows,
        skipped_before,
        skipped_after: suffix.saturating_sub(3),
        capped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn h06_only_http_schemes_pass_iced_click_gate() {
        for url in [
            "https://example.com",
            "HTTP://example.com",
            "https:relative",
        ] {
            assert!(http_link(url));
        }
        for url in [
            "file:///tmp/a",
            "mailto:a@example.com",
            "javascript:alert(1)",
            "//example.com",
            " https://example.com",
            "command:run",
            "ééé:",
        ] {
            assert!(!http_link(url));
        }
    }
    #[test]
    fn h03_iced_history_restores_fifty_blocks() {
        assert_eq!(history_start(199), 0);
        assert_eq!(history_start(200), 50);
        assert_eq!(history_start(5000), 4850);
        assert_eq!(earlier_label(4850), "Show earlier · 4850 blocks hidden");
    }
    #[test]
    fn h08_large_edits_expand_all_lines_without_changing_payload() {
        let edit = EditDiff {
            path: "a.rs".into(),
            old: String::new(),
            new: (0..60).map(|i| format!("line {i}\n")).collect(),
        };
        let diff = edit_lines(&edit);
        assert_eq!(diff.shown(false).len(), 40);
        assert_eq!(diff.more_label(false).as_deref(), Some("Show all 60 lines"));
        assert_eq!(diff.shown(true).len(), 60);
        assert!(diff.more_label(true).is_none());
        assert_eq!(diff.rows.last().unwrap().text, "line 59");
    }
    #[test]
    fn h08_iced_context_lcs_and_cap() {
        let edit = EditDiff {
            path: "a".into(),
            old: "a\nb\nc\nd\ne\nold\nf\ng\nh\ni\n".into(),
            new: "a\nb\nc\nd\ne\nnew\nf\ng\nh\ni\n".into(),
        };
        let diff = edit_lines(&edit);
        assert_eq!((diff.skipped_before, diff.skipped_after), (2, 1));
        assert_eq!(
            diff.rows.iter().map(|r| r.sign).collect::<String>(),
            "   -+   "
        );
        let capped = edit_lines(&EditDiff {
            new: "x\n".repeat(401),
            ..edit
        });
        assert!(capped.capped);
    }
}

pub fn edit_title(row: &crate::model::Row, index: usize) -> String {
    let Some(tool) = &row.tool else {
        return String::new();
    };
    if tool.name == "Write" {
        return match tool.output.as_ref().filter(|_| !tool.is_error) {
            None => "write",
            Some(output)
                if output
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|word| word.eq_ignore_ascii_case("created")) =>
            {
                "new file"
            }
            Some(_) => "overwrite",
        }
        .into();
    }
    let count = row.edits().len();
    let mut title = if count > 1 {
        format!("edit {} of {count}", index + 1)
    } else {
        "edit".into()
    };
    let input = if tool.name == "MultiEdit" {
        &tool.input["edits"][index]
    } else {
        &tool.input
    };
    if input["replace_all"] == true {
        title.push_str(" · replace all");
    }
    title
}

#[cfg(test)]
mod edit_contract_tests {
    use super::*;
    use crate::{
        events::{Entry, HarnessEvent, ItemKind, Record},
        model::ChatModel,
    };
    fn row(name: &str, input: serde_json::Value) -> crate::model::Row {
        ChatModel::from_records(&[Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::ItemStarted {
                id: "edit".into(),
                kind: ItemKind::ToolCall {
                    name: name.into(),
                    input,
                },
            }),
        }])
        .rows
        .remove(0)
    }
    #[test]
    fn h08_invalid_edit_shapes_keep_json_and_write_labels_follow_results() {
        for (name, input) in [
            (
                "Edit",
                serde_json::json!({"old_string":"a","new_string":"b"}),
            ),
            (
                "MultiEdit",
                serde_json::json!({"file_path":"a","edits":[{"old_string":"a","new_string":"b"},{"old_string":"a"}]}),
            ),
        ] {
            assert!(row(name, input).edits().is_empty());
        }
        let mut write = row(
            "Write",
            serde_json::json!({"file_path":"a","content":"new"}),
        );
        assert_eq!(edit_title(&write, 0), "write");
        write.tool.as_mut().unwrap().output = Some("Successfully created file".into());
        assert_eq!(edit_title(&write, 0), "new file");
        write.tool.as_mut().unwrap().output = Some("Updated existing file".into());
        assert_eq!(edit_title(&write, 0), "overwrite");
        let edit = row(
            "Edit",
            serde_json::json!({"file_path":"a","old_string":"a","new_string":"b","replace_all":true}),
        );
        assert_eq!(edit_title(&edit, 0), "edit · replace all");
    }
}
