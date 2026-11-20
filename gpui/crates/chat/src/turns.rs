//! Presentation grouping for user turns. This never mutates the chat projection.
use crate::model::{Row, RowKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Turn {
    pub user_row: usize,
    pub end: usize,
}

impl Turn {
    pub fn contains(&self, row: usize) -> bool {
        self.user_row < row && row < self.end
    }
}

pub(super) fn group(rows: &[Row]) -> Vec<Turn> {
    let starts: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| (row.kind == RowKind::User).then_some(index))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(position, &user_row)| Turn {
            user_row,
            end: starts.get(position + 1).copied().unwrap_or(rows.len()),
        })
        .collect()
}

pub(super) fn containing(turns: &[Turn], row: usize) -> Option<&Turn> {
    turns.iter().find(|turn| turn.contains(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Entry, HarnessEvent, Record};
    use crate::model::ChatModel;

    #[test]
    fn groups_each_user_prompt_with_following_tool_rows_without_mutating_model() {
        let records = [
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("first".into()),
            },
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::ItemStarted {
                    id: "tool-1".into(),
                    kind: crate::events::ItemKind::ToolCall {
                        name: "Read".into(),
                        input: serde_json::json!({"file_path":"a.rs"}),
                    },
                }),
            },
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("second".into()),
            },
        ];
        let model = ChatModel::from_records(&records);
        let before = model.rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let turns = group(&model.rows);
        assert_eq!(turns.len(), 2);
        assert!(turns[0].contains(1));
        assert!(!turns[1].contains(2));
        assert_eq!(
            model.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn folded_turn_membership_excludes_user_and_next_turn() {
        let records = [
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("first".into()),
            },
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta("reply".into())),
            },
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("second".into()),
            },
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta("later".into())),
            },
        ];
        let model = ChatModel::from_records(&records);
        let turns = group(&model.rows);
        assert_eq!(turns.len(), 2);
        assert!(!turns[0].contains(0));
        assert!(turns[0].contains(1));
        assert!(!turns[0].contains(2));
        assert!(turns[1].contains(3));
    }
}
