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

/// Iced folds work before the final answer, leaving substantial prose and attention visible.
pub(super) struct WorkFold {
    pub range: std::ops::Range<usize>,
    pub label: String,
}

pub(super) fn work_folds(model: &crate::model::ChatModel) -> Vec<WorkFold> {
    let mut folds = Vec::new();
    for turn in &model.completed_turns {
        let mut segments = model.rows[turn.rows.clone()].split(|row| row.kind == RowKind::User);
        let split = model.rows[turn.rows.clone()]
            .iter()
            .any(|row| row.kind == RowKind::User);
        let label = if !split {
            turn.duration_ms
                .map(|ms| format!("Worked for {}", crate::composer::elapsed(ms)))
        } else {
            None
        }
        .unwrap_or_else(|| "Worked".into());
        let mut offset = turn.rows.start;
        for segment in segments.by_ref() {
            if let Some(last) = segment
                .iter()
                .rposition(|row| matches!(row.kind, RowKind::Thinking | RowKind::Tool))
            {
                let mut first = None;
                let mut has_work = false;
                for index in 0..=last + 1 {
                    let row = segment.get(index).filter(|_| index <= last);
                    let boundary = row.is_none_or(|row| {
                        !row.scope.is_empty()
                            || !matches!(
                                row.kind,
                                RowKind::Assistant | RowKind::Thinking | RowKind::Tool
                            )
                            || row.tool.as_ref().is_some_and(|tool| {
                                tool.is_error
                                    || (matches!(tool.name.as_str(), "Agent" | "Task")
                                        && tool.output.is_none())
                            })
                            || (row.kind == RowKind::Tool
                                && model.rows.iter().any(|other| {
                                    other.request.as_ref().is_some_and(|r| !r.resolved)
                                }))
                            || substantial(row)
                    });
                    if boundary {
                        if let Some(start) = first.take()
                            && has_work
                        {
                            folds.push(WorkFold {
                                range: offset + start..offset + index,
                                label: label.clone(),
                            });
                        }
                        has_work = false;
                    } else if let Some(row) = row {
                        first.get_or_insert(index);
                        has_work |= matches!(row.kind, RowKind::Thinking | RowKind::Tool);
                    }
                }
            }
            offset += segment.len() + 1;
        }
    }
    folds
}

fn substantial(row: &Row) -> bool {
    if row.kind != RowKind::Assistant {
        return false;
    }
    if row.body.len() >= 280 {
        return true;
    }
    markdown::to_mdast(&row.body, &markdown::ParseOptions::gfm()).map_or(true, |node| {
        node.children().is_some_and(|children| {
            children.iter().any(|node| {
                matches!(
                    node,
                    markdown::mdast::Node::Heading(_)
                        | markdown::mdast::Node::Table(_)
                        | markdown::mdast::Node::List(_)
                        | markdown::mdast::Node::Blockquote(_)
                        | markdown::mdast::Node::Code(_)
                )
            })
        })
    })
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
