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
    // Tool cards whose request still waits, or waited when its turn ended.
    let held: std::collections::HashSet<usize> = model
        .rows
        .iter()
        .filter(|row| waits(row))
        .filter_map(|row| row.request.as_ref()?.tool_row)
        .collect();
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
            for range in fold_segment(segment, offset, &held) {
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
/// a withdrawn prompt is removed, a request shows only in the footer panel.
/// They go wherever the block before them goes.
fn follows_previous(row: &Row) -> bool {
    !row.scope.is_empty() || row.hidden()
}

/// Iced's `foldSegment`: the groups of one segment that fold, or none.
/// `first` is the segment's first row index; `held` are tool rows a waiting
/// request keeps out.
fn fold_segment(
    segment: &[Row],
    first: usize,
    held: &std::collections::HashSet<usize>,
) -> Vec<std::ops::Range<usize>> {
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
        if keeps_out_of_fold(row, held.contains(&(first + index))) || substantial(row) {
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

/// Iced's "a card whose request still waits": not answered, and not closed by
/// the harness before its turn ended (Iced folds before clearing requests).
fn waits(row: &Row) -> bool {
    row.decision.is_none()
        && row
            .request
            .as_ref()
            .is_some_and(|r| !r.resolved || r.waiting_at_turn_end)
}

/// Iced's `keepsOutOfFold`: error blocks, warnings, delegation cards, a failed tool, a
/// running subagent's card and a card whose request still waits.
fn keeps_out_of_fold(row: &Row, held: bool) -> bool {
    match row.kind {
        // Iced: `.error-block, .dlg, .note.warn`.
        RowKind::Error | RowKind::Warning | RowKind::Delegation => true,
        RowKind::Tool => {
            held || row.tool.as_ref().is_some_and(|tool| {
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
    /// Iced folds an answered request's card and its tool with the turn; a
    /// request still waiting when the turn ends keeps both out, because Iced
    /// folds before it clears the turn's requests.
    #[test]
    fn answered_request_folds_and_one_waiting_at_turn_end_stays_out() {
        use crate::events::{ItemKind, RuntimeRequestKind, TurnStatus};
        let harness = |event| Record {
            at_ms: None,
            entry: Entry::Harness(event),
        };
        let started = |id: &str, name: &str| {
            harness(HarnessEvent::ItemStarted {
                id: id.into(),
                kind: ItemKind::ToolCall {
                    name: name.into(),
                    input: serde_json::json!({}),
                },
            })
        };
        let completed = |id: &str| {
            harness(HarnessEvent::ItemCompleted {
                id: id.into(),
                output: "ok".into(),
                is_error: false,
            })
        };
        let model = |answered: bool| {
            let mut records = vec![
                Record {
                    at_ms: None,
                    entry: Entry::UserPrompt("Go".into()),
                },
                started("one", "Bash"),
                completed("one"),
                started("ask", "AskUserQuestion"),
                harness(HarnessEvent::RuntimeRequest {
                    request_id: "q".into(),
                    tool_use_id: "ask".into(),
                    kind: RuntimeRequestKind::Question {
                        questions: serde_json::json!([{"question": "Which?"}]),
                    },
                }),
            ];
            if answered {
                records.push(harness(HarnessEvent::RuntimeRequestResolved {
                    request_id: "q".into(),
                }));
                records.push(completed("ask"));
            }
            records.extend([
                started("two", "Bash"),
                completed("two"),
                harness(HarnessEvent::TextDelta("Done.".into())),
                harness(HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: serde_json::Value::Null,
                    cost_usd: None,
                    rate_limit: None,
                }),
            ]);
            ChatModel::from_records(&records)
        };
        let ranges = |model: &ChatModel| {
            work_folds(model)
                .into_iter()
                .map(|fold| fold.range)
                .collect::<Vec<_>>()
        };
        // Rows: 0 prompt, 1 Bash, 2 AskUserQuestion, 3 question, 4 Bash, 5 text.
        let answered = model(true);
        assert_eq!(answered.rows[3].kind, RowKind::Question);
        assert_eq!(ranges(&answered), vec![1..5]);
        let waiting = model(false);
        assert_eq!(waiting.rows[3].kind, RowKind::Question);
        assert!(waiting.rows[3].request.as_ref().unwrap().resolved);
        assert_eq!(ranges(&waiting), [1..2, 4..5]);
    }
}
