//! Iced's literal PR-create recognition, shared without changing its scanner.
use crate::chat::{tool_input::ToolInputTracker, types::HarnessEvent};
use crate::merge_plan::{self, PrRef};
use std::collections::HashMap;

/// Shared PR recognition for live facts and worker/Claude transcript adapters.
/// Keep the command paired with its own output; URL mentions alone are not facts.
pub fn created_prs(command: &str, output: &str) -> Vec<PrRef> {
    if is_gh_pr_create(command) {
        merge_plan::pr_urls_in(output)
    } else {
        Vec::new()
    }
}

/// Whether the shell actually invokes `gh pr create`, including chains,
/// environment assignments, executable paths and command substitutions.
/// Quoting preserves literal executable tokens; quoted phrases, comments and
/// heredoc bodies are not separate command words.
pub fn is_gh_pr_create(command: &str) -> bool {
    pr_create_command_kind(command) == Some(true)
}

/// Unknown wrapper options cannot establish a literal create invocation.
fn pr_create_command_kind(command: &str) -> Option<bool> {
    let commands = ShellScanner(command.chars().peekable(), 0, false).commands(false)?;
    let mut unknown = false;
    for command in &commands {
        match shell_command_creates_pr(command) {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    (!unknown).then_some(false)
}

fn shell_command_creates_pr(command: &ShellCommand) -> Option<bool> {
    let words = &command.words;
    let mut at = 0;
    let mut env_assignments = false;
    while let Some(word) = words.get(at) {
        if command.assignments[at] || (env_assignments && shell_assignment(word)) {
            at += 1;
            continue;
        }
        if command.quoted[at] && word.rsplit('/').next() != Some("gh") {
            return Some(false);
        }
        if matches!(
            word.as_str(),
            "if" | "while" | "until" | "then" | "do" | "else" | "elif" | "!" | "{"
        ) {
            at += 1;
            continue;
        }
        let executable = word.rsplit('/').next().unwrap_or(word);
        at += 1;
        if executable == "gh" {
            return Some(
                words.get(at).map(String::as_str) == Some("pr")
                    && words.get(at + 1).map(String::as_str) == Some("create")
                    && !command.quoted[at]
                    && !command.quoted[at + 1],
            );
        }
        if !matches!(
            executable,
            "env" | "command" | "builtin" | "exec" | "time" | "nohup"
        ) {
            return Some(false);
        }
        env_assignments = executable == "env";
        while let Some(option) = words.get(at).filter(|word| word.starts_with('-')) {
            at += 1;
            if option == "--" {
                break;
            }
            match (executable, option.as_str()) {
                ("command", "-v" | "-V") | ("env", "--help" | "--version") => {
                    return Some(false);
                }
                ("env", "-i" | "-v" | "-" | "--ignore-environment" | "--debug")
                | ("command", "-p")
                | ("exec", "-c" | "-l" | "-cl" | "-lc")
                | ("time", "-p" | "-l") => {}
                ("env", "-u" | "-C" | "-P" | "--unset" | "--chdir") | ("exec", "-a") => {
                    words.get(at)?;
                    at += 1;
                }
                ("env", option)
                    if option.starts_with("--unset=")
                        || option.starts_with("--chdir=")
                        || (["-u", "-C", "-P"].iter().any(|prefix| {
                            option.starts_with(prefix) && option.len() > prefix.len()
                        }))
                        || (option.starts_with('-')
                            && option.len() > 1
                            && option[1..].chars().all(|ch| matches!(ch, 'i' | 'v'))) => {}
                // shortcut: env -S and dynamic shell evaluation need their own
                // argument parser; do not treat their text as command words.
                _ => return None,
            }
        }
    }
    Some(false)
}

pub fn shell_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().enumerate().all(|(i, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (i > 0 && byte.is_ascii_digit())
            })
    })
}

#[derive(Default)]
pub struct ShellCommand {
    pub nested: bool,
    pub environment: bool,
    pub separator: String,
    pub words: Vec<String>,
    pub quoted: Vec<bool>,
    pub assignments: Vec<bool>,
    pub stdout_redirected: bool,
}

#[derive(Default)]
struct ShellWord {
    text: String,
    quoted: bool,
    assignment: bool,
    commands: Vec<ShellCommand>,
}

/// A single lexical pass: the same quote/comment/continuation state governs
/// both words and heredocs. Nested substitutions get their own shell context.
/// shortcut: this identifies literal invocations, not full Bash execution;
/// dynamic executables, backticks and arithmetic require a full shell parser.
// The final flag records syntax outside simple commands, pipelines and lists.
pub struct ShellScanner<'a>(
    pub std::iter::Peekable<std::str::Chars<'a>>,
    pub usize,
    pub bool,
);

impl ShellScanner<'_> {
    pub fn commands(&mut self, nested: bool) -> Option<Vec<ShellCommand>> {
        // Transcript input must not exhaust the stack through shell nesting.
        self.1 += 1;
        if self.1 > 64 {
            return None;
        }
        let mut commands = Vec::new();
        let mut current = ShellCommand::default();
        let mut children = Vec::new();
        let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
        while let Some(&ch) = self.0.peek() {
            match ch {
                ch if ch.is_whitespace() && ch != '\n' => {
                    if !matches!(ch, ' ' | '\t') {
                        self.2 = true;
                    }
                    self.0.next();
                }
                '#' => {
                    while self.0.peek().is_some_and(|ch| *ch != '\n') {
                        self.0.next();
                    }
                }
                '\n' => {
                    self.0.next();
                    current.separator = ch.to_string();
                    finish_shell_command(&mut commands, &mut current, &mut children);
                    for (delimiter, strip_tabs, quoted) in heredocs.drain(..) {
                        self.heredoc(&delimiter, strip_tabs, quoted)?;
                    }
                }
                ';' | '&' | '|' => {
                    self.0.next();
                    if ch == '&' && self.0.peek() == Some(&'>') {
                        self.redirect(&mut current, &mut heredocs, Some(1))?;
                    } else {
                        if current.words.is_empty() {
                            self.2 = true;
                        }
                        current.separator = ch.to_string();
                        if self.0.peek() == Some(&ch) {
                            self.0.next();
                            current.separator.push(ch);
                        }
                        finish_shell_command(&mut commands, &mut current, &mut children);
                    }
                }
                '(' => {
                    self.2 = true;
                    self.0.next();
                    children.extend(self.commands(true)?);
                }
                ')' => {
                    self.0.next();
                    if !nested || !heredocs.is_empty() {
                        return None;
                    }
                    finish_shell_command(&mut commands, &mut current, &mut children);
                    self.1 -= 1;
                    return Some(commands);
                }
                '<' | '>' => self.redirect(&mut current, &mut heredocs, None)?,
                _ => {
                    let word = self.word(true)?;
                    if !word.quoted
                        && !word.text.is_empty()
                        && word.text.bytes().all(|byte| byte.is_ascii_digit())
                        && matches!(self.0.peek(), Some('<' | '>'))
                    {
                        self.redirect(&mut current, &mut heredocs, Some(word.text.parse().ok()?))?;
                    } else {
                        children.extend(word.commands);
                        current.nested = nested;
                        current.environment |= word.assignment && current.words.is_empty();
                        if (!word.text.is_empty() || word.quoted)
                            && !(word.assignment && current.words.is_empty())
                        {
                            current.words.push(word.text);
                            current.quoted.push(word.quoted);
                            current.assignments.push(word.assignment);
                        }
                    }
                }
            }
        }
        if nested || !heredocs.is_empty() {
            return None;
        }
        finish_shell_command(&mut commands, &mut current, &mut children);
        self.1 -= 1;
        Some(commands)
    }

    fn word(&mut self, expand: bool) -> Option<ShellWord> {
        let mut word = ShellWord::default();
        let mut quote = None;
        while let Some(&ch) = self.0.peek() {
            if quote.is_none() && (ch.is_whitespace() || ";&|()<>".contains(ch)) {
                break;
            }
            self.0.next();
            if quote == Some('\'') {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.text.push(ch);
                }
            } else if ch == '\\' {
                let next = self.0.next()?;
                if next != '\n' {
                    if quote == Some('"') && !"$`\"\\".contains(next) {
                        word.text.push('\\');
                    }
                    word.text.push(next);
                    word.quoted = true;
                }
            } else if Some(ch) == quote {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                quote = Some(ch);
                word.quoted = true;
            } else if expand && ch == '$' && self.0.peek() == Some(&'(') {
                self.2 = true;
                self.0.next();
                if self.0.peek() == Some(&'(') {
                    return None;
                }
                word.commands.extend(self.commands(true)?);
                // Expansion cannot manufacture a literal executable or subcommand.
                word.text.push('\0');
            } else if expand && ch == '`' {
                return None;
            } else {
                if expand && ch == '$' {
                    self.2 = true;
                }
                word.text.push(ch);
                if ch == '=' && !word.quoted && shell_assignment(&word.text) {
                    word.assignment = true;
                }
            }
        }
        quote.is_none().then_some(word)
    }

    fn redirect(
        &mut self,
        current: &mut ShellCommand,
        heredocs: &mut Vec<(String, bool, bool)>,
        fd: Option<u32>,
    ) -> Option<()> {
        self.2 = true;
        let operator = self.0.next()?;
        let mut heredoc = false;
        let mut strip_tabs = false;
        if self.0.peek() == Some(&operator) {
            self.0.next();
            if operator == '<' {
                if self.0.peek() == Some(&'<') {
                    self.0.next(); // Consume the entire here-string operator.
                } else {
                    heredoc = true;
                    if self.0.peek() == Some(&'-') {
                        self.0.next();
                        strip_tabs = true;
                    }
                }
            }
        }
        if matches!(self.0.peek(), Some('&' | '|')) {
            self.0.next();
        }
        while matches!(self.0.peek(), Some(' ' | '\t')) {
            self.0.next();
        }
        let target = self.word(!heredoc)?;
        if target.text.is_empty() && !target.quoted {
            return None;
        }
        if operator == '>' && fd.unwrap_or(1) == 1 {
            current.stdout_redirected = true;
        }
        if heredoc {
            heredocs.push((target.text, strip_tabs, target.quoted));
        }
        Some(())
    }

    fn heredoc(&mut self, delimiter: &str, strip_tabs: bool, quoted: bool) -> Option<()> {
        loop {
            let mut line = String::new();
            loop {
                let ch = self.0.next();
                if ch.is_none() || ch == Some('\n') {
                    // Only unquoted heredocs join escaped physical lines.
                    if ch.is_some()
                        && !quoted
                        && line.chars().rev().take_while(|ch| *ch == '\\').count() % 2 == 1
                    {
                        line.pop();
                        continue;
                    }
                    break;
                }
                line.push(ch?);
            }
            if (if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            }) == delimiter
            {
                return Some(());
            }
            self.0.peek()?;
        }
    }
}

fn finish_shell_command(
    commands: &mut Vec<ShellCommand>,
    current: &mut ShellCommand,
    children: &mut Vec<ShellCommand>,
) {
    for child in children.iter_mut() {
        child.stdout_redirected |= current.stdout_redirected;
    }
    if !current.words.is_empty() || current.environment {
        commands.push(std::mem::take(current));
    } else {
        *current = ShellCommand::default();
    }
    commands.append(children);
}

/// Tool input and its own successful result must agree; mentions are not PRs.
#[derive(Default)]
pub struct CreatedPrs {
    tracker: ToolInputTracker,
    commands: HashMap<String, String>,
}
impl CreatedPrs {
    pub fn observe(&mut self, event: &HarnessEvent) -> Vec<PrRef> {
        if let Some(call) = self.tracker.observe_calls(event, |name| name == "Bash") {
            if let Some(command) = call
                .input
                .get("command")
                .and_then(serde_json::Value::as_str)
            {
                self.commands.insert(call.id, command.to_string());
            }
        }
        self.results(event)
    }
    fn results(&mut self, event: &HarnessEvent) -> Vec<PrRef> {
        match event {
            HarnessEvent::SubagentEvent { event, .. } => self.results(event),
            HarnessEvent::ItemCompleted {
                id,
                output,
                is_error,
            } => self
                .commands
                .remove(id)
                .filter(|_| !is_error)
                .map(|command| created_prs(&command, output))
                .unwrap_or_default(),
            HarnessEvent::ProcessExited { .. } => {
                self.commands.clear();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::types::ItemKind;
    #[test]
    fn live_pr_create_observer_uses_iceds_literal_scanner() {
        for (command, creates) in [
            ("env EXAMPLE=1 gh pr create --title example", true),
            ("echo 'gh pr create'", false),
            ("gh pr view 53", false),
            ("cat <<'EOF'\ngh pr create\nEOF", false),
        ] {
            assert_eq!(is_gh_pr_create(command), creates, "{command}");
            for is_error in [false, true] {
                let mut observer = CreatedPrs::default();
                assert!(observer
                    .observe(&HarnessEvent::ItemStarted {
                        id: "tool".into(),
                        kind: ItemKind::ToolCall {
                            name: "Bash".into(),
                            input: serde_json::json!({"command":command})
                        }
                    })
                    .is_empty());
                let found = observer.observe(&HarnessEvent::ItemCompleted {
                    id: "tool".into(),
                    output: "https://github.com/owner/repo/pull/53".into(),
                    is_error,
                });
                assert_eq!(found.len(), usize::from(creates && !is_error));
            }
        }
    }
}
