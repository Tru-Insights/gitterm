use crate::{
    adapter::Receipt,
    events::RuntimeRequestKind,
    intents::{ChatIntent, PermissionDecision, Question, QuestionAnswer},
    model::Row,
    view::IntentCallback,
};
use gpui::{
    AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, px,
};
use gpui_component::{
    Disableable, Selectable, Sizable,
    button::Button,
    input::{Input, InputEvent, InputState},
};

struct AnswerDraft {
    selected: Vec<String>,
    other_on: bool,
    other: Entity<InputState>,
}

/// Retained separately from rich row views so virtual row eviction never loses answers.
pub(crate) struct CardView {
    row: Row,
    callback: Option<IntentCallback>,
    read_only: bool,
    pub(super) preview: bool,
    questions: Result<Vec<Question>, String>,
    drafts: Vec<AnswerDraft>,
    deny_reason: Entity<InputState>,
    submitted: Option<ChatIntent>,
    receipt: Option<Receipt>,
    _subscriptions: Vec<Subscription>,
}
impl CardView {
    pub fn new(
        row: Row,
        callback: Option<IntentCallback>,
        read_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let questions = match &row.request.as_ref().expect("request card").kind {
            RuntimeRequestKind::Question { questions } => {
                serde_json::from_value::<Vec<Question>>(questions.clone())
                    .map_err(|error| format!("Cannot answer malformed questionnaire: {error}"))
            }
            _ => Ok(vec![]),
        };
        let mut subscriptions = vec![];
        let drafts = questions
            .as_ref()
            .ok()
            .into_iter()
            .flatten()
            .map(|_| {
                let other = cx.new(|cx| InputState::new(window, cx).placeholder("Your answer"));
                subscriptions.push(cx.subscribe(&other, |_, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
                AnswerDraft {
                    selected: vec![],
                    other_on: false,
                    other,
                }
            })
            .collect();
        let deny_reason =
            cx.new(|cx| InputState::new(window, cx).placeholder("Reason for denying (optional)"));
        subscriptions.push(
            cx.subscribe_in(&deny_reason, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.deny(cx);
                }
            }),
        );
        Self {
            deny_reason,
            preview: false,
            row,
            callback,
            read_only,
            questions,
            drafts,
            submitted: None,
            receipt: None,
            _subscriptions: subscriptions,
        }
    }
    pub fn set_callback(&mut self, callback: Option<IntentCallback>, cx: &mut Context<Self>) {
        self.callback = callback;
        cx.notify();
    }
    pub fn matches_request(&self, row: &Row) -> bool {
        self.row.scope == row.scope
            && self
                .row
                .request
                .as_ref()
                .zip(row.request.as_ref())
                .is_some_and(|(old, new)| {
                    old.id == new.id && old.kind == new.kind && old.identity == new.identity
                })
    }
    pub fn sync(&mut self, row: Row, read_only: bool, cx: &mut Context<Self>) {
        self.row = row;
        self.read_only = read_only;
        cx.notify();
    }
    pub fn update_receipt(&mut self, receipt: &Receipt, cx: &mut Context<Self>) {
        if self
            .receipt
            .as_ref()
            .is_some_and(|old| old.accepts_update(receipt))
            && !self.row.request.as_ref().expect("request").resolved
        {
            self.receipt = Some(receipt.clone());
            if receipt.state.retryable() {
                self.submitted = None;
            }
            cx.notify();
        }
    }
    fn enabled(&self) -> bool {
        !self.read_only
            && self.callback.is_some()
            && self.submitted.is_none()
            && self.row.decision.is_none()
            && !self.row.request.as_ref().expect("request").resolved
            && self.row.scope.is_empty()
    }
    pub(super) fn emit(&mut self, intent: ChatIntent, cx: &mut Context<Self>) {
        let request = self.row.request.as_ref().expect("request");
        if !self.enabled() || intent.request_id() != Some(request.id.as_str()) {
            return;
        }
        match (&intent, &request.kind) {
            (
                ChatIntent::Permission { decision, .. },
                RuntimeRequestKind::Permission { suggestions, .. },
            ) => {
                if let PermissionDecision::AllowAlways { suggestion } = decision
                    && (self.preview
                        || !offered_options(suggestions)
                            .iter()
                            .any(|(offered, _)| *offered == suggestion))
                {
                    return;
                }
            }
            (
                ChatIntent::AnswerQuestion { .. } | ChatIntent::DismissQuestion { .. },
                RuntimeRequestKind::Question { .. },
            ) => {}
            _ => return,
        }
        self.submitted = Some(intent.clone());
        let intent = if let Some(identity) = &request.identity {
            ChatIntent::Answer {
                identity: identity.clone(),
                answer: Box::new(intent),
            }
        } else {
            intent
        };
        let receipt = (self.callback.as_ref().expect("enabled callback"))(intent, cx);
        self.receipt = Some(receipt.clone());
        self.update_receipt(&receipt, cx);
        cx.notify();
    }
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.emit(
            ChatIntent::DismissQuestion {
                request_id: self.row.request.as_ref().expect("request").id.clone(),
            },
            cx,
        );
    }
    fn deny(&mut self, cx: &mut Context<Self>) {
        self.permission(
            PermissionDecision::Deny {
                reason: self.deny_reason.read(cx).value().to_string(),
            },
            cx,
        );
    }
    fn permission(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        self.emit(
            ChatIntent::Permission {
                request_id: self.row.request.as_ref().unwrap().id.clone(),
                decision,
            },
            cx,
        );
    }
    fn choose(&mut self, index: usize, label: Option<String>, cx: &mut Context<Self>) {
        if !self.enabled() {
            return;
        }
        let Ok(questions) = &self.questions else {
            return;
        };
        let Some(question) = questions.get(index) else {
            return;
        };
        let draft = &mut self.drafts[index];
        if let Some(label) = label {
            if !question.options.iter().any(|option| option.label == label) {
                return;
            }
            if question.multi_select {
                if draft.selected.contains(&label) {
                    draft.selected.retain(|value| value != &label);
                } else {
                    draft.selected.push(label);
                }
            } else {
                draft.selected = vec![label];
                draft.other_on = false;
            }
        } else {
            draft.other_on = !draft.other_on;
            if draft.other_on && !question.multi_select {
                draft.selected.clear();
            }
        }
        cx.notify();
    }
    fn answers(&self, cx: &gpui::App) -> Option<Vec<QuestionAnswer>> {
        let questions = self.questions.as_ref().ok()?;
        if questions.is_empty() {
            return None;
        }
        questions
            .iter()
            .zip(&self.drafts)
            .map(|(question, draft)| {
                let selected: Vec<_> = question
                    .options
                    .iter()
                    .filter(|option| draft.selected.contains(&option.label))
                    .map(|option| option.label.clone())
                    .collect();
                let text = draft.other.read(cx).value().trim().to_owned();
                let free_text = (draft.other_on && !text.is_empty()).then_some(text);
                if selected.is_empty() && free_text.is_none() {
                    return None;
                }
                Some(QuestionAnswer {
                    question: question.question.clone(),
                    selected,
                    free_text,
                })
            })
            .collect()
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if let Some(answers) = self.answers(cx) {
            self.emit(
                ChatIntent::AnswerQuestion {
                    request_id: self.row.request.as_ref().unwrap().id.clone(),
                    answers,
                },
                cx,
            );
        }
    }
}
fn answer_label(intent: &ChatIntent) -> String {
    match intent {
        ChatIntent::Permission { decision, .. } => match decision {
            PermissionDecision::Allow => "Allow".into(),
            PermissionDecision::AllowAlways { .. } => "Allow always".into(),
            PermissionDecision::Deny { .. } => "Deny".into(),
        },
        ChatIntent::AnswerQuestion { answers, .. } => answers
            .iter()
            .map(|answer| {
                let mut values = answer.selected.clone();
                values.extend(answer.free_text.clone());
                format!("{}: {}", answer.question, values.join(", "))
            })
            .collect::<Vec<_>>()
            .join("; "),
        ChatIntent::DismissQuestion { .. } => "Dismissed".into(),
        _ => unreachable!("only card intents are submitted"),
    }
}
fn offered_options(suggestions: &[serde_json::Value]) -> Vec<(&serde_json::Value, String)> {
    suggestions
        .iter()
        .find(|value| value["destination"] == "localSettings")
        .map(|value| (value, "Allow and remember".into()))
        .into_iter()
        .chain(
            suggestions
                .iter()
                .filter(|value| value["type"] == "acp_option")
                .map(|value| {
                    (
                        value,
                        value["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("Always allow")
                            .to_owned(),
                    )
                }),
        )
        .collect()
}

fn permission_salient(tool: &str, input: &serde_json::Value) -> String {
    let key = match tool {
        "Bash" => Some("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" => Some("file_path"),
        "NotebookEdit" => Some("notebook_path"),
        _ => None,
    };
    key.and_then(|key| input[key].as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| serde_json::to_string_pretty(input).expect("JSON value"))
}

fn describe_rule(value: &serde_json::Value) -> String {
    if let Some(rules) = value["rules"].as_array() {
        return rules
            .iter()
            .map(|rule| {
                let name = rule["toolName"].as_str().unwrap_or("");
                match rule["ruleContent"]
                    .as_str()
                    .filter(|content| !content.is_empty())
                {
                    Some(content) => format!("{name}({content})"),
                    None => name.to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
    }
    if let Some(mode) = value["mode"].as_str() {
        return format!("mode {mode}");
    }
    value["type"].as_str().unwrap_or("").to_owned()
}
impl Render for CardView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div().flex().flex_col().gap(px(6.));
        if self.row.request.as_ref().expect("request").resolved {
            return content
                .child(if self.row.decision.is_some() {
                    "Recorded answer above"
                } else {
                    "Resolved · decision not recorded"
                })
                .into_any_element();
        }
        if let Some(answer) = &self.submitted {
            return content
                .child(format!(
                    "{} · {}",
                    self.receipt
                        .as_ref()
                        .map(|r| r.state.label())
                        .unwrap_or_else(|| "Preparing command".into()),
                    answer_label(answer)
                ))
                .into_any_element();
        }
        if !self.enabled() {
            return content
                .child(if self.row.decision.is_some() {
                    "Recorded answer above"
                } else if self.row.request.as_ref().unwrap().resolved {
                    "Resolved · decision not recorded"
                } else if !self.row.scope.is_empty() {
                    "Worker request · read-only"
                } else {
                    "Read-only chat"
                })
                .into_any_element();
        }
        if let Some(receipt) = &self.receipt {
            content = content.child(receipt.state.label());
        }
        match &self.row.request.as_ref().unwrap().kind {
            RuntimeRequestKind::Permission {
                tool_name,
                input,
                suggestions,
                decision_reason,
            } => {
                content = content.child(format!("Allow {tool_name}?"));
                if let Some(description) = input["description"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                {
                    content = content.child(description.to_owned());
                }
                if let Some(reason) = decision_reason.as_ref().filter(|value| !value.is_empty()) {
                    content = content.child(reason.clone());
                }
                content = content.child(permission_salient(tool_name, input));
                content = content.child(Button::new("allow").small().label("Allow once").on_click(
                    cx.listener(|this, _, _, cx| this.permission(PermissionDecision::Allow, cx)),
                ));
                for (index, (suggestion, label)) in
                    offered_options(suggestions).into_iter().enumerate()
                {
                    let suggestion = suggestion.clone();
                    let tooltip = if suggestion["destination"] == "localSettings" {
                        format!(
                            "Saves {} to .claude/settings.local.json",
                            describe_rule(&suggestion)
                        )
                    } else {
                        "Answers with the agent's own option".into()
                    };
                    content = content.child(
                        Button::new(("always", index))
                            .small()
                            .label(label)
                            .tooltip(tooltip)
                            .disabled(self.preview)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.permission(
                                    PermissionDecision::AllowAlways {
                                        suggestion: suggestion.clone(),
                                    },
                                    cx,
                                )
                            })),
                    );
                }
                content = content.child(
                    div()
                        .id("deny-reason")
                        .debug_selector(|| "deny-reason".into())
                        .child(Input::new(&self.deny_reason)),
                );
                if self.preview && !offered_options(suggestions).is_empty() {
                    content = content.child("not saved in preview");
                }
                content = content.child(
                    Button::new("deny")
                        .small()
                        .label("Deny")
                        .on_click(cx.listener(|this, _, _, cx| this.deny(cx))),
                );
            }
            RuntimeRequestKind::Question { .. } => {
                match &self.questions {
                    Err(error) => {
                        content = content.child(error.clone());
                    }
                    Ok(questions) => {
                        content = content.child(if questions.len() == 1 {
                            "Claude has a question".into()
                        } else {
                            format!("Claude has {} questions", questions.len())
                        });
                        for (index, question) in questions.iter().enumerate() {
                            let draft = &self.drafts[index];
                            if !question.header.is_empty() {
                                content = content.child(question.header.clone());
                            }
                            content = content.child(question.question.clone());
                            if question.multi_select {
                                content = content.child("choose any");
                            }
                            for (oi, option) in question.options.iter().enumerate() {
                                let label = option.label.clone();
                                content = content.child(
                                    Button::new(("option", index * 1000 + oi))
                                        .small()
                                        .h_auto()
                                        .min_h(px(24.))
                                        .py(px(4.))
                                        .debug_selector(move || format!("question-option-{index}-{oi}"))
                                        .accessibility_label(if option.description.is_empty() {
                                            label.clone()
                                        } else {
                                            format!("{label}: {}", option.description)
                                        })
                                        .child(div().flex().flex_col().w_full().items_start().whitespace_normal()
                                            .child(label.clone())
                                            .children((!option.description.is_empty()).then(|| div()
                                                .debug_selector(move || format!("question-option-description-{index}-{oi}"))
                                                .w_full()
                                                .text_size(px(11.))
                                                .line_height(px(16.))
                                                .child(option.description.clone()))))
                                        .selected(draft.selected.contains(&label))
                                        .toggled(draft.selected.contains(&label))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.choose(index, Some(label.clone()), cx)
                                        })),
                                );
                            }
                            content = content.child(
                                Button::new(("other", index))
                                    .small()
                                    .label("Other…")
                                    .selected(draft.other_on)
                                    .toggled(draft.other_on)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.choose(index, None, cx);
                                        if this.enabled() && this.drafts[index].other_on {
                                            this.drafts[index]
                                                .other
                                                .read(cx)
                                                .focus_handle(cx)
                                                .focus(window, cx);
                                        }
                                    })),
                            );
                            if draft.other_on {
                                content = content.child(Input::new(&draft.other));
                            }
                        }
                        let disabled = self.answers(cx).is_none();
                        content = content.child(
                            Button::new("submit")
                                .small()
                                .label("Submit")
                                .disabled(disabled)
                                .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                        );
                    }
                }
                content = content.child(
                    Button::new("dismiss")
                        .small()
                        .label("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
                );
            }
        }
        content
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Enter, window, cx| {
                    if this
                        .deny_reason
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                    {
                        window.prevent_default();
                        cx.stop_propagation();
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "cards_tests.rs"]
mod tests;
