//! Iced's consecutive-tool disclosure, independent of persisted conversation rows.
use crate::model::{Row, RowKind, Tool};
use std::{collections::HashSet, ops::Range};

pub(super) struct Run {
    pub range: Range<usize>,
    pub label: String,
}

pub(super) fn group(rows: &[Row], completed: &[crate::model::CompletedTurn]) -> Vec<Run> {
    let pending_scopes: HashSet<_> = rows
        .iter()
        .filter(|row| row.request.as_ref().is_some_and(|r| !r.resolved))
        .map(|row| row.scope.as_str())
        .collect();
    let eligible = |row: &Row| {
        row.kind == RowKind::Tool
            && row.tool.as_ref().is_some_and(|tool| !tool.is_error)
            && !pending_scopes.contains(row.scope.as_str())
            && !row.tool.as_ref().is_some_and(|tool| {
                matches!(tool.name.as_str(), "Agent" | "Task") && tool.output.is_none()
            })
    };
    let mut runs = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        if !eligible(&rows[index]) {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < rows.len() && rows[index].scope == rows[start].scope && eligible(&rows[index])
        {
            index += 1;
        }
        let tools: Vec<_> = rows[start..index]
            .iter()
            .filter_map(|row| row.tool.as_ref())
            .collect();
        let mut label = run_label(&tools);
        if !completed
            .iter()
            .any(|turn| turn.rows.start <= start && index <= turn.rows.end)
            && let Some(tool) = tools.iter().rev().find(|tool| tool.output.is_none())
        {
            label.push_str(" · now: ");
            label.push_str(salient(tool).unwrap_or(&tool.name));
        }
        runs.push(Run {
            range: start..index,
            label,
        });
    }
    runs
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
fn salient(tool: &Tool) -> Option<&str> {
    let keys: &[&str] = match tool.name.as_str() {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" | "MultiEdit" => &["file_path"],
        "NotebookEdit" => &["notebook_path"],
        "Grep" | "Glob" => &["pattern"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        "Agent" | "Task" => &["description"],
        _ => &[
            "command",
            "file_path",
            "filePath",
            "path",
            "url",
            "query",
            "pattern",
            "title",
        ],
    };
    field(tool, keys)
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
        let model = ChatModel::from_records(&records);
        let runs = group(&model.rows, &model.completed_turns);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].range, 0..3);
        assert_eq!(runs[0].label, "Ran 3 commands · now: three");
        assert_eq!(runs[1].label, "Read 1 file · now: a.rs");
    }
}
