use crate::{
    adapter::Receipt,
    events::RuntimeRequestKind,
    intents::{ChatIntent, PermissionDecision, Question, QuestionAnswer},
    model::Row,
    view::IntentCallback,
};
use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, px,
};
use gpui_component::{
    Disableable, Sizable,
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
    questions: Result<Vec<Question>, String>,
    drafts: Vec<AnswerDraft>,
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
        Self {
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
                .is_some_and(|(old, new)| old.id == new.id && old.kind == new.kind)
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
        if !self.enabled() {
            return;
        }
        self.submitted = Some(intent.clone());
        let receipt = (self.callback.as_ref().expect("enabled callback"))(intent, cx);
        self.receipt = Some(receipt.clone());
        self.update_receipt(&receipt, cx);
        cx.notify();
    }
    fn permission(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        // Validate offered rules at the UI boundary, including programmatic invocations.
        let RuntimeRequestKind::Permission { suggestions, .. } =
            &self.row.request.as_ref().unwrap().kind
        else {
            return;
        };
        if let PermissionDecision::AllowAlways { suggestion } = &decision
            && !suggestions
                .iter()
                .any(|offered| offered == suggestion && rememberable(offered))
        {
            return;
        }
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
            PermissionDecision::Deny => "Deny".into(),
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
fn rememberable(value: &serde_json::Value) -> bool {
    value["destination"] == "localSettings" || value["type"] == "acp_option"
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
            RuntimeRequestKind::Permission { suggestions, .. } => {
                content = content.child(Button::new("allow").small().label("Allow").on_click(
                    cx.listener(|this, _, _, cx| this.permission(PermissionDecision::Allow, cx)),
                ));
                for (index, suggestion) in suggestions
                    .iter()
                    .filter(|value| rememberable(value))
                    .enumerate()
                {
                    let suggestion = suggestion.clone();
                    let label = suggestion["name"]
                        .as_str()
                        .map(|name| format!("Allow always · {name}"))
                        .unwrap_or_else(|| "Allow always".into());
                    content = content.child(
                        Button::new(("always", index))
                            .small()
                            .label(format!("{label} · unavailable until destination verified"))
                            .disabled(true)
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
                content = content.child(Button::new("deny").small().label("Deny").on_click(
                    cx.listener(|this, _, _, cx| this.permission(PermissionDecision::Deny, cx)),
                ));
            }
            RuntimeRequestKind::Question { .. } => match &self.questions {
                Err(error) => {
                    content = content.child(error.clone());
                }
                Ok(questions) => {
                    for (index, question) in questions.iter().enumerate() {
                        let draft = &self.drafts[index];
                        content = content.child(format!(
                            "{} · {}{}",
                            question.header,
                            question.question,
                            if question.multi_select {
                                " (choose any)"
                            } else {
                                ""
                            }
                        ));
                        for (oi, option) in question.options.iter().enumerate() {
                            let label = option.label.clone();
                            content = content.child(
                                Button::new(("option", index * 1000 + oi))
                                    .small()
                                    .label(format!(
                                        "{} {} {}",
                                        if draft.selected.contains(&label) {
                                            "✓"
                                        } else {
                                            "○"
                                        },
                                        label,
                                        option.description
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.choose(index, Some(label.clone()), cx)
                                    })),
                            );
                        }
                        content = content.child(
                            Button::new(("other", index))
                                .small()
                                .label(if draft.other_on {
                                    "✓ Other…"
                                } else {
                                    "Other…"
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.choose(index, None, cx)),
                                ),
                        );
                        if draft.other_on {
                            content = content.child(Input::new(&draft.other));
                        }
                    }
                    let disabled = self.answers(cx).is_none();
                    content = content.child(
                        Button::new("submit")
                            .small()
                            .label("Submit answers")
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    );
                    content =
                        content.child(Button::new("dismiss").small().label("Dismiss").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.emit(
                                    ChatIntent::DismissQuestion {
                                        request_id: this.row.request.as_ref().unwrap().id.clone(),
                                    },
                                    cx,
                                )
                            }),
                        ));
                }
            },
        }
        content.into_any_element()
    }
}

#[cfg(test)]
#[path = "cards_tests.rs"]
mod tests;
