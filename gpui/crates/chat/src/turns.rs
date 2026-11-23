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

/// Iced's turn fold (agent_chat.html "turn folds (TRU-209)"): a finished turn
/// folds everything from its prompt up to its last tool call or thinking.
/// Narration stays inside the fold; what needs the human and substantial
/// answers stay out and split it.
pub(super) struct WorkFold {
    pub range: std::ops::Range<usize>,
    pub label: String,
}

pub(super) fn work_folds(model: &crate::model::ChatModel) -> Vec<WorkFold> {
    let pending = model
        .rows
        .iter()
        .any(|row| row.request.as_ref().is_some_and(|r| !r.resolved));
    let mut folds = Vec::new();
    for turn in &model.completed_turns {
        let rows = &model.rows[turn.rows.clone()];
        // shortcut: a turn split by a mid-turn message has one duration and no
        // per-segment split times, so its folds say "Worked" (Iced times each).
        let split = rows.iter().any(|row| row.kind == RowKind::User);
        let label = turn
            .duration_ms
            .filter(|_| !split)
            .map(|ms| format!("Worked for {}", crate::composer::elapsed(ms)))
            .unwrap_or_else(|| "Worked".into());
        let mut offset = turn.rows.start;
        // A message sent mid-turn ends one fold and starts the next.
        for segment in rows.split(|row| row.kind == RowKind::User) {
            for range in fold_segment(segment, pending) {
                folds.push(WorkFold {
                    range: offset + range.start..offset + range.end,
                    label: label.clone(),
                });
            }
            offset += segment.len() + 1;
        }
    }
    folds
}

/// Iced's `WORK`: a top-level tool card or thinking block.
pub(super) fn is_work(row: &Row) -> bool {
    row.scope.is_empty() && matches!(row.kind, RowKind::Thinking | RowKind::Tool)
}

/// Rows Iced has no top-level block for: a subagent's rows live in its card,
/// a withdrawn prompt is removed. They go wherever the block before them goes.
fn follows_previous(row: &Row) -> bool {
    !row.scope.is_empty() || row.kind == RowKind::Withdrawn
}

/// Iced's `foldSegment`: the groups of one segment that fold, or none.
fn fold_segment(segment: &[Row], pending: bool) -> Vec<std::ops::Range<usize>> {
    let Some(last) = segment.iter().rposition(is_work) else {
        return Vec::new(); // a reply with no work: nothing to fold
    };
    let end = last
        + 1
        + segment[last + 1..]
            .iter()
            .take_while(|row| follows_previous(row))
            .count();
    let mut groups = Vec::new();
    let mut start = None;
    for (index, row) in segment[..end].iter().enumerate() {
        if follows_previous(row) {
            continue;
        }
        if keeps_out_of_fold(row, pending) || substantial(row) {
            if let Some(start) = start.take() {
                groups.push(start..index);
            }
            continue;
        }
        start.get_or_insert(index);
        // Iced appends a visual reply after its tool card, outside the fold.
        if row
            .tool
            .as_ref()
            .is_some_and(|tool| super::visual::visual_request(tool).is_some())
            && let Some(start) = start.take()
        {
            groups.push(start..index + 1);
        }
    }
    if let Some(start) = start {
        groups.push(start..end);
    }
    if !groups
        .iter()
        .any(|range| segment[range.clone()].iter().any(is_work))
    {
        return Vec::new();
    }
    groups
}

/// Iced's `keepsOutOfFold`: error blocks, delegation cards, a failed tool, a
/// running subagent's card and a card whose request still waits.
fn keeps_out_of_fold(row: &Row, pending: bool) -> bool {
    match row.kind {
        // Iced shows requests in its panel, not the timeline. GPUI's request
        // cards record the human's decision, so they never fold.
        RowKind::Error | RowKind::Delegation | RowKind::Permission | RowKind::Question => true,
        // shortcut: rows do not link a request to its tool card, so any
        // waiting request keeps every tool card out; fine while requests
        // rarely outlive their turn.
        RowKind::Tool => {
            pending
                || row.tool.as_ref().is_some_and(|tool| {
                    tool.is_error
                        || (matches!(tool.name.as_str(), "Agent" | "Task") && tool.output.is_none())
                })
        }
        _ => row.delivery.is_some() || row.delegation.is_some(),
    }
}

/// Iced's `isSubstantialAssistantText`: an answer long or structured enough
/// to stay visible.
fn substantial(row: &Row) -> bool {
    if row.kind != RowKind::Assistant {
        return false;
    }
    // JavaScript string length: UTF-16 code units.
    if row.body.encode_utf16().count() >= 280 {
        return true;
    }
    // Iced keeps text it cannot parse visible rather than hide an answer.
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
    fn turn_grouping_ignores_withdrawn() {
        let echo = |id: &str| Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::UserMessageEcho {
                id: Some(id.into()),
                text: id.into(),
                image_count: 0,
                delivery: None,
            }),
        };
        let lifecycle = |id: &str, state| Record {
            at_ms: None,
            entry: Entry::Harness(HarnessEvent::MessageLifecycle {
                id: id.into(),
                state,
            }),
        };
        let records = [
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("first".into()),
            },
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta("reply".into())),
            },
            echo("withdrawn"),
            lifecycle("withdrawn", crate::events::MessageState::Cancelled),
            Record {
                at_ms: None,
                entry: Entry::Harness(HarnessEvent::TextDelta("more".into())),
            },
            // Started, a stale cancel cannot withdraw it (core and Iced agree).
            echo("sent"),
            lifecycle("sent", crate::events::MessageState::Started),
            lifecycle("sent", crate::events::MessageState::Cancelled),
        ];
        let model = ChatModel::from_records(&records);
        assert_eq!(model.rows[2].kind, RowKind::Withdrawn);
        let turns = group(&model.rows);
        assert_eq!(
            turns,
            [
                Turn {
                    user_row: 0,
                    end: 4
                },
                Turn {
                    user_row: 4,
                    end: 5
                }
            ]
        );
        assert!(turns[0].contains(2) && turns[0].contains(3));
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
