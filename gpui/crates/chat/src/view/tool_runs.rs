//! Iced's consecutive-tool disclosure, independent of persisted conversation rows.
use crate::model::{Row, RowKind, Tool};
use std::{collections::HashSet, ops::Range};

pub(super) struct Run {
    pub range: Range<usize>,
    pub label: String,
}

pub(super) fn group(model: &crate::model::ChatModel, folds: &[super::turns::WorkFold]) -> Vec<Run> {
    let (rows, completed) = (&model.rows, &model.completed_turns);
    let finished = |index: usize| completed.iter().any(|turn| turn.rows.contains(&index));
    // Iced's `foldNodes`: in a finished turn's fold, everything between two
    // narration paragraphs that has a tool card is one run; outside a fold a
    // finished turn has no runs.
    let mut runs = Vec::new();
    for fold in folds {
        let mut start = fold.range.start;
        for index in fold.range.start..=fold.range.end {
            let narration = rows
                .get(index)
                .is_some_and(|row| row.scope.is_empty() && row.kind == RowKind::Assistant);
            if index < fold.range.end && !narration {
                continue;
            }
            let tools: Vec<_> = rows[start..index]
                .iter()
                .filter(|row| row.scope.is_empty())
                .filter_map(|row| row.tool.as_ref())
                .collect();
            if !tools.is_empty() {
                runs.push(Run {
                    range: start..index,
                    label: run_label(&tools),
                });
            }
            start = index + 1;
        }
    }
    // Iced's `refreshLiveRuns`: only in the turn in flight, and not until
    // a live event follows a replay. Its `keepsOutOfFold` takes out a failed
    // tool, a running subagent's card and the card a waiting request is for.
    let Some(live) = model.live_turn_start() else {
        runs.sort_by_key(|run| run.range.start);
        return runs;
    };
    let held: HashSet<usize> = rows
        .iter()
        .filter_map(|row| row.request.as_ref())
        .filter(|request| !request.resolved)
        .filter_map(|request| request.tool_row)
        .collect();
    let eligible = |index: usize| {
        let row = &rows[index];
        row.kind == RowKind::Tool
            && row.scope.is_empty()
            && !held.contains(&index)
            && row.tool.as_ref().is_some_and(|tool| {
                !tool.is_error
                    && !tool
                        .sub
                        .as_ref()
                        .is_some_and(|sub| sub.state == crate::model::SubState::Running)
            })
    };
    // Rows Iced has no block for (a request, a withdrawn prompt) do not
    // break a run.
    let skipped = |index: usize| rows[index].hidden() || !rows[index].scope.is_empty();
    let mut index = live;
    while index < rows.len() {
        if finished(index) || !eligible(index) {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 1;
        index += 1;
        while index < rows.len() && !finished(index) && (skipped(index) || eligible(index)) {
            index += 1;
            if !skipped(index - 1) {
                end = index;
            }
        }
        index = end;
        let tools: Vec<_> = rows[start..end]
            .iter()
            .filter_map(|row| row.tool.as_ref())
            .collect();
        let mut label = run_label(&tools);
        if let Some(tool) = tools.iter().rev().find(|tool| tool.output.is_none()) {
            label.push_str(" · now: ");
            label.push_str(salient(tool).unwrap_or(&tool.name));
        }
        runs.push(Run {
            range: start..end,
            label,
        });
    }
    runs.sort_by_key(|run| run.range.start);
    runs
}

pub(super) fn tooltip(rows: &[Row]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    // Iced names the run's own calls, not a subagent's inside its card.
    let scope = rows.first().map(|row| row.scope.as_str());
    for tool in rows
        .iter()
        .filter(|row| Some(row.scope.as_str()) == scope)
        .filter_map(|row| row.tool.as_ref())
    {
        if let Some((_, n)) = counts.iter_mut().find(|(name, _)| *name == tool.name) {
            *n += 1;
        } else {
            counts.push((&tool.name, 1));
        }
    }
    counts
        .into_iter()
        .map(|(name, n)| {
            if n > 1 {
                format!("{name} ×{n}")
            } else {
                name.into()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn kind(tool: &Tool) -> &str {
    match tool.name.to_lowercase().as_str() {
        "bash" | "shell" | "run_shell_command" | "execute" | "exec" | "terminal" | "bashoutput"
        | "killshell" => "command",
        "read" | "read_file" | "readfile" | "read_many_files" | "notebookread" | "view" => "read",
        "edit" | "multiedit" | "write" | "notebookedit" | "write_file" | "edit_file"
        | "replace" | "apply_patch" | "patch" => "edit",
        "grep"
        | "glob"
        | "search"
        | "find"
        | "list"
        | "ls"
        | "list_directory"
        | "websearch"
        | "web_search"
        | "google_web_search"
        | "search_file_content"
        | "codebase_search" => "search",
        _ => match tool.input["kind"].as_str() {
            Some("execute") => "command",
            Some("read") => "read",
            Some("edit" | "delete" | "move") => "edit",
            Some("search") => "search",
            _ => "other",
        },
    }
}

fn field<'a>(tool: &'a Tool, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| tool.input[*key].as_str().filter(|s| !s.is_empty()))
}
pub(super) fn salient(tool: &Tool) -> Option<&str> {
    crate::model::salient(&tool.name, &tool.input)
}
fn run_label(tools: &[&Tool]) -> String {
    let mut counts = [0; 5];
    let mut read = HashSet::new();
    let mut edit = HashSet::new();
    let mut others = HashSet::new();
    for (index, tool) in tools.iter().enumerate() {
        let kind = kind(tool);
        if matches!(kind, "read" | "edit") {
            let path = field(tool, &["file_path", "filePath", "notebook_path", "path"])
                .or_else(|| tool.input["paths"][0].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("#{index}"));
            if kind == "read" {
                read.insert(path);
            } else {
                edit.insert(path);
            }
        } else {
            counts[match kind {
                "command" => 0,
                "search" => 3,
                _ => 4,
            }] += 1;
            if kind == "other" {
                others.insert(tool.name.as_str());
            }
        }
    }
    counts[1] = read.len();
    counts[2] = edit.len();
    let mut parts = Vec::new();
    for (index, (verb, noun)) in [
        ("ran", "command"),
        ("read", "file"),
        ("edited", "file"),
        ("searched", "time"),
    ]
    .into_iter()
    .enumerate()
    {
        let n = counts[index];
        if n > 0 {
            parts.push(format!(
                "{verb} {n} {noun}{}",
                if n == 1 { "" } else { "s" }
            ));
        }
    }
    let n = counts[4];
    if n > 0 {
        parts.push(if !parts.is_empty() {
            format!("{n} other action{}", if n == 1 { "" } else { "s" })
        } else if others.len() == 1 {
            format!(
                "used {}{}",
                others.into_iter().next().unwrap(),
                if n > 1 {
                    format!(" {n} times")
                } else {
                    String::new()
                }
            )
        } else {
            format!("used {n} tools")
        });
    }
    let mut text = match parts.pop() {
        Some(last) if !parts.is_empty() => format!("{} and {last}", parts.join(", ")),
        Some(last) => last,
        None => return String::new(),
    };
    text[0..1].make_ascii_uppercase();
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{events::parse_fixture, model::ChatModel};

    #[test]
    fn consecutive_commands_make_one_iced_summary_and_narration_breaks_it() {
        let records = parse_fixture(r#"{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Bash","input":{"command":"one"}}}}
{"type":"item_completed","data":{"id":"a","output":"","is_error":false}}
{"type":"item_started","data":{"id":"b","kind":{"type":"tool_call","name":"Bash","input":{"command":"two"}}}}
{"type":"item_started","data":{"id":"c","kind":{"type":"tool_call","name":"Bash","input":{"command":"three"}}}}
{"type":"text_delta","data":"reply"}
{"type":"item_started","data":{"id":"d","kind":{"type":"tool_call","name":"Read","input":{"file_path":"a.rs"}}}}"#).unwrap();
        // Live, in a running turn: Iced's refreshLiveRuns.
        let mut model = ChatModel::default();
        model.apply(&crate::events::Record {
            at_ms: None,
            entry: crate::events::Entry::UserPrompt("Go".into()),
        });
        for record in &records {
            model.apply(record);
        }
        let runs = group(&model, &super::super::turns::work_folds(&model));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].range, 1..4);
        assert_eq!(runs[0].label, "Ran 3 commands · now: three");
        assert_eq!(runs[1].label, "Read 1 file · now: a.rs");
    }

    fn live(records: &[crate::events::Record]) -> ChatModel {
        let mut model = ChatModel::default();
        for record in records {
            model.apply(record);
        }
        model
    }
    fn ranges(model: &ChatModel) -> Vec<Range<usize>> {
        group(model, &super::super::turns::work_folds(model))
            .into_iter()
            .map(|run| run.range)
            .collect()
    }

    /// G04: a waiting request takes only its own tool (or its subagent's
    /// Agent card) out of a live run; the hidden request row does not break it.
    #[test]
    fn g04_only_the_requested_tool_leaves_a_live_run() {
        use crate::events::{Entry, HarnessEvent, Record, RuntimeRequestKind};
        use crate::model::subagent_tests::{harness, inside, tool};
        let bash = || serde_json::json!({"command": "check"});
        let request = |id: &str| HarnessEvent::RuntimeRequest {
            request_id: id.into(),
            tool_use_id: id.into(),
            kind: RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: serde_json::json!({}),
                suggestions: vec![],
                decision_reason: None,
            },
        };
        let prompt = Record {
            at_ms: None,
            entry: Entry::UserPrompt("Go".into()),
        };
        let model = live(&[
            prompt.clone(),
            harness(tool("a", "Bash", bash())),
            harness(tool("b", "Bash", bash())),
            harness(request("b")),
            harness(tool("c", "Bash", bash())),
            harness(tool("d", "Bash", bash())),
        ]);
        // Rows: 0 prompt, 1 a, 2 b (waiting), 3 request, 4 c, 5 d.
        assert_eq!(ranges(&model), [1..2, 4..6]);
        // A subagent's request holds its Agent card; a resolved one holds nothing.
        let mut model = live(&[
            prompt,
            harness(tool("a", "Bash", bash())),
            harness(tool("agent", "Agent", serde_json::json!({}))),
            harness(inside(&["agent"], tool("x", "Bash", bash()))),
            harness(inside(&["agent"], request("x"))),
            harness(crate::model::subagent_tests::done("agent", false)),
            harness(tool("c", "Bash", bash())),
        ]);
        // Rows: 0 prompt, 1 a, 2 Agent, 3 request, 4 c.
        assert_eq!(ranges(&model), [1..2, 4..5]);
        model.apply(&harness(inside(
            &["agent"],
            HarnessEvent::RuntimeRequestResolved {
                request_id: "x".into(),
            },
        )));
        assert_eq!(ranges(&model), vec![Range { start: 1, end: 5 }]);
    }

    /// F09: a replayed running turn has no live runs (Iced's refreshLiveRuns
    /// skips a replay) until the next live event, and nothing groups outside
    /// a turn.
    #[test]
    fn f09_replayed_running_turn_groups_tools_only_after_a_live_event() {
        use crate::events::{Entry, HarnessEvent, Record};
        use crate::model::subagent_tests::{harness, tool};
        let bash = || serde_json::json!({"command": "check"});
        let records = [
            Record {
                at_ms: None,
                entry: Entry::UserPrompt("Go".into()),
            },
            harness(tool("a", "Bash", bash())),
            harness(tool("b", "Bash", bash())),
        ];
        let mut model = ChatModel::from_records(&records);
        assert!(ranges(&model).is_empty());
        model.apply(&harness(HarnessEvent::TextDelta("Still".into())));
        assert_eq!(ranges(&model), vec![Range { start: 1, end: 3 }]);
        assert!(ranges(&live(&records[1..])).is_empty(), "no turn, no runs");
    }
}
