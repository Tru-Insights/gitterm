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
    /// Iced's red `N failed`: the segment's failed tool cards, which stay
    /// outside its folds.
    pub failed: usize,
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
        let mut offset = turn.rows.start;
        // A message sent mid-turn ends one fold and starts the next; each
        // segment is timed from its own start to its own end.
        for (position, segment) in rows.split(|row| row.kind == RowKind::User).enumerate() {
            let label = match turn.segments.get(position) {
                Some((Some(start), Some(end))) => format!(
                    "Worked for {}",
                    crate::composer::elapsed(end.saturating_sub(*start))
                ),
                _ => "Worked".into(),
            };
            let failed = segment
                .iter()
                .filter(|row| {
                    row.scope.is_empty()
                        && !row.hidden()
                        && row.tool.as_ref().is_some_and(|tool| tool.is_error)
                })
                .count();
            for range in fold_segment(segment, offset, &held) {
                folds.push(WorkFold {
                    range: offset + range.start..offset + range.end,
                    label: label.clone(),
                    failed,
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

/// Iced's `keepsOutOfFold`: error blocks, warnings, delegation cards, visual
/// replies, a failed tool, a
/// running subagent's card and a card whose request still waits.
fn keeps_out_of_fold(row: &Row, held: bool) -> bool {
    match row.kind {
        // Iced: `.error-block, .dlg, .note.warn, .html-render`.
        RowKind::Error | RowKind::Warning | RowKind::Delegation | RowKind::Visual => true,
        RowKind::Tool => {
            held || row.tool.as_ref().is_some_and(|tool| {
                tool.is_error || tool.sub.as_ref().is_some_and(|sub| sub.running_at_turn_end)
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

    fn timing(start_ms: Option<u64>, end_ms: Option<u64>, split_ms: Option<u64>) -> Record {
        Record {
            at_ms: None,
            entry: Entry::TurnTiming {
                start_ms,
                end_ms,
                split_ms,
            },
        }
    }
    fn prompt(text: &str) -> Record {
        Record {
            at_ms: None,
            entry: Entry::UserPrompt(text.into()),
        }
    }
    fn folds(model: &ChatModel) -> Vec<(std::ops::Range<usize>, String, usize)> {
        work_folds(model)
            .into_iter()
            .map(|fold| (fold.range, fold.label, fold.failed))
            .collect()
    }

    /// F05, F07: a mid-turn prompt splits the turn; each segment is timed
    /// from its own start (the split marker's time) and every fold in it
    /// carries the segment's failed count, though failed tools stay out.
    #[test]
    fn f05_f07_each_segment_has_its_own_duration_and_failed_count() {
        use crate::model::subagent_tests::{done, harness, tool, turn_done};
        let bash = || serde_json::json!({"command": "check"});
        let records = [
            prompt("Go"),
            timing(Some(1_000), None, None),
            harness(tool("a", "Bash", bash())),
            harness(done("a", false)),
            harness(tool("b", "Bash", bash())),
            harness(done("b", true)),
            harness(tool("c", "Bash", bash())),
            harness(done("c", false)),
            timing(None, None, Some(4_000)),
            prompt("Also this"),
            harness(tool("d", "Bash", bash())),
            harness(done("d", false)),
            harness(crate::events::HarnessEvent::TextDelta("Done.".into())),
            timing(None, Some(70_500), None),
            harness(turn_done(crate::events::TurnStatus::Completed)),
        ];
        let model = ChatModel::from_records(&records);
        // Rows: 0 prompt, 1–3 tools (2 failed), 4 prompt, 5 tool, 6 text.
        assert_eq!(
            folds(&model),
            [
                (1..2, "Worked for 3.0s".into(), 1),
                (3..4, "Worked for 3.0s".into(), 1),
                (5..6, "Worked for 1m 6s".into(), 0),
            ]
        );
        // Without markers both segments say "Worked".
        let untimed: Vec<_> = records
            .iter()
            .filter(|record| !matches!(record.entry, Entry::TurnTiming { .. }))
            .cloned()
            .collect();
        let labels: Vec<_> = folds(&ChatModel::from_records(&untimed))
            .into_iter()
            .map(|(_, label, _)| label)
            .collect();
        assert_eq!(labels, ["Worked", "Worked", "Worked"]);
    }

    /// F04: Iced keeps out the card of a subagent still running when the
    /// turn ends (a background one, whose Agent call already returned); a
    /// foreground one that ended with its call folds. The card stays out
    /// after the subagent finishes, as Iced's fold does not move.
    #[test]
    fn f04_background_subagent_running_at_turn_end_stays_out_of_the_fold() {
        use crate::events::{HarnessEvent, TurnStatus};
        use crate::model::subagent_tests::{done, harness, inside, tool, turn_done};
        let bash = || serde_json::json!({"command": "check"});
        let turn = |tracked: bool| {
            let mut records = vec![
                prompt("Go"),
                harness(tool("one", "Bash", bash())),
                harness(done("one", false)),
                harness(tool("agent", "Agent", serde_json::json!({}))),
            ];
            if tracked {
                records.push(harness(inside(
                    &["agent"],
                    HarnessEvent::TurnStarted {
                        session_id: None,
                        model: None,
                    },
                )));
            }
            records.extend([
                harness(done("agent", false)),
                harness(tool("two", "Bash", bash())),
                harness(done("two", false)),
                harness(HarnessEvent::TextDelta("Done.".into())),
                harness(turn_done(TurnStatus::Completed)),
            ]);
            ChatModel::from_records(&records)
        };
        let ranges = |model: &ChatModel| {
            folds(model)
                .into_iter()
                .map(|(range, ..)| range)
                .collect::<Vec<_>>()
        };
        // Rows: 0 prompt, 1 Bash, 2 Agent, 3 Bash, 4 text.
        assert_eq!(ranges(&turn(false)), [1..4]);
        let mut background = turn(true);
        assert_eq!(ranges(&background), [1..2, 3..4]);
        background.apply(&harness(inside(
            &["agent"],
            turn_done(TurnStatus::Completed),
        )));
        assert_eq!(ranges(&background), [1..2, 3..4]);
    }

    /// F08: a process exit folds the turn in flight, and (as Iced folds
    /// before clearing requests) a request still waiting keeps its card out.
    #[test]
    fn f08_process_exit_folds_and_keeps_a_waiting_request_out() {
        use crate::events::{HarnessEvent, RuntimeRequestKind};
        use crate::model::subagent_tests::{done, harness, tool};
        let bash = || serde_json::json!({"command": "check"});
        let records = [
            prompt("Go"),
            harness(tool("one", "Bash", bash())),
            harness(done("one", false)),
            harness(tool("two", "Bash", bash())),
            harness(HarnessEvent::RuntimeRequest {
                request_id: "r".into(),
                tool_use_id: "two".into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: serde_json::json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            }),
            harness(tool("three", "Bash", bash())),
            harness(done("three", false)),
            harness(HarnessEvent::ProcessExited { code: Some(1) }),
        ];
        let model = ChatModel::from_records(&records);
        // Rows: 0 prompt, 1 Bash, 2 Bash (waiting), 3 request, 4 Bash.
        let ranges: Vec<_> = folds(&model).into_iter().map(|(r, ..)| r).collect();
        assert_eq!(ranges, [1..2, 4..5]);
    }

    /// F03: a warning note (Iced's `.note.warn`) stays out and splits the fold.
    #[test]
    fn f03_warning_note_stays_out_of_the_fold() {
        use crate::events::{HarnessEvent, TurnStatus};
        use crate::model::subagent_tests::{done, harness, tool, turn_done};
        let bash = || serde_json::json!({"command": "check"});
        let model = ChatModel::from_records(&[
            prompt("Go"),
            harness(tool("one", "Bash", bash())),
            harness(done("one", false)),
            harness(HarnessEvent::MessageWithdrawRefused {
                id: "m".into(),
                reason: "already sent".into(),
            }),
            harness(tool("two", "Bash", bash())),
            harness(done("two", false)),
            harness(turn_done(TurnStatus::Completed)),
        ]);
        assert_eq!(model.rows[2].kind, RowKind::Warning);
        let ranges: Vec<_> = folds(&model).into_iter().map(|(r, ..)| r).collect();
        assert_eq!(ranges, [1..2, 3..4]);
    }

    /// Y02: a visual reply is its own block, appended when the result
    /// arrives (after what came in meanwhile), and it stays out of the fold.
    #[test]
    fn y02_visual_reply_is_its_own_block_at_completion_and_stays_out() {
        use crate::events::{HarnessEvent, TurnStatus};
        use crate::model::subagent_tests::{harness, tool, turn_done};
        let html = serde_json::json!({"html": "<h1>Chart</h1>", "height": 240});
        let output = serde_json::json!({ "html_render": html }).to_string();
        let model = ChatModel::from_records(&[
            prompt("Go"),
            harness(tool("v", "html_render", html.clone())),
            harness(tool("b", "Bash", serde_json::json!({"command": "check"}))),
            harness(HarnessEvent::ItemCompleted {
                id: "v".into(),
                output,
                is_error: false,
            }),
            harness(crate::model::subagent_tests::done("b", false)),
            harness(tool("c", "Bash", serde_json::json!({"command": "check"}))),
            harness(crate::model::subagent_tests::done("c", false)),
            harness(HarnessEvent::TextDelta("Done.".into())),
            harness(turn_done(TurnStatus::Completed)),
        ]);
        // Rows: 0 prompt, 1 html_render, 2 Bash, 3 visual, 4 Bash, 5 text.
        let kinds: Vec<_> = model.rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::User,
                RowKind::Tool,
                RowKind::Tool,
                RowKind::Visual,
                RowKind::Tool,
                RowKind::Assistant
            ]
        );
        let ranges: Vec<_> = folds(&model).into_iter().map(|(r, ..)| r).collect();
        assert_eq!(ranges, [1..3, 4..5]);
    }
}
