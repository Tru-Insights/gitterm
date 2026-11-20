//! Iced's agent_chat.html catalog projection; values are never optimistic.
use gitterm_core::chat::{
    state::{AgentEvent, State},
    types::{HarnessEvent, SettingChange},
};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picker {
    Model,
    Effort,
    Mode,
}
impl Picker {
    pub fn from_chip(id: &str) -> Option<Self> {
        match id {
            "model" => Some(Self::Model),
            "effort" => Some(Self::Effort),
            "mode" => Some(Self::Mode),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    pub section: &'static str,
    pub label: String,
    pub description: String,
    pub change: SettingChange,
    pub selected: bool,
    pub disabled: bool,
}
#[derive(Debug, Default)]
pub struct Catalog {
    pub models: Vec<OptionRow>,
    pub efforts: Vec<OptionRow>,
    pub modes: Vec<OptionRow>,
}
fn base(value: &str) -> &str {
    value.strip_suffix("[1m]").unwrap_or(value)
}
pub fn effort_name(value: Option<&str>) -> &str {
    match value {
        None => "Auto",
        Some("low") => "Low",
        Some("medium") => "Medium",
        Some("high") => "High",
        Some("xhigh") => "X-High",
        Some("max") => "Max",
        Some(value) => value,
    }
}
pub fn mode_name(value: &str) -> &str {
    match value {
        "default" => "Default",
        "acceptEdits" => "Accept edits",
        "plan" => "Plan mode",
        "auto" => "Auto mode",
        "dontAsk" => "Don't ask",
        "bypassPermissions" => "Bypass permissions",
        value => value,
    }
}
impl Catalog {
    pub fn options(&self, picker: Picker) -> &[OptionRow] {
        match picker {
            Picker::Model => &self.models,
            Picker::Effort => &self.efforts,
            Picker::Mode => &self.modes,
        }
    }
    pub fn offers(&self, change: &SettingChange) -> bool {
        self.models
            .iter()
            .chain(&self.efforts)
            .chain(&self.modes)
            .any(|row| !row.disabled && &row.change == change)
    }
    pub fn project(state: &State, acp: bool) -> Self {
        let ready = state
            .conversation
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::Harness(HarnessEvent::Ready { models, modes, .. }) => {
                    Some((models, modes))
                }
                _ => None,
            });
        let mut catalog = Self::default();
        let mut models = ready.map(|(models, _)| models.clone()).unwrap_or_default();
        let listed = !models.is_empty();
        if !listed && !acp {
            models = [
                json!({"value":"default", "displayName":"Default", "description":"Your Claude settings decide"}),
                json!({"value":"opus", "displayName":"Opus"}),
                json!({"value":"sonnet", "displayName":"Sonnet"}),
                json!({"value":"haiku", "displayName":"Haiku"}),
                json!({"value":"fable", "displayName":"Fable"}),
            ].into();
        }
        models.retain(|m| m["value"].is_string());
        if let Some(current) = state.acknowledged.model.as_deref() {
            if !models.iter().any(|m| m["value"] == current) {
                models.insert(0, json!({"value":current, "displayName":current}));
            }
            let wide = current.ends_with("[1m]");
            let variant = |value: &str| {
                let want = format!("{}[1m]", base(value));
                models.iter().any(|m| m["value"] == want).then_some(want)
            };
            for info in &models {
                let value = info["value"].as_str().unwrap();
                if value.ends_with("[1m]") && models.iter().any(|m| m["value"] == base(value)) {
                    continue;
                }
                catalog.models.push(OptionRow {
                    section: "Model",
                    label: info["displayName"].as_str().unwrap_or(value).into(),
                    description: info["description"].as_str().unwrap_or_default().into(),
                    change: SettingChange::Model(if wide {
                        variant(value).unwrap_or_else(|| value.into())
                    } else {
                        value.into()
                    }),
                    selected: base(current) == value || current == value,
                    disabled: false,
                });
            }
            let info = models.iter().find(|m| m["value"] == current);
            let supported = if listed {
                info.is_none_or(|m| m["supportsEffort"] == true)
            } else {
                !acp
            };
            let effort = state
                .acknowledged
                .effort
                .as_ref()
                .and_then(|e| e.effort.as_deref());
            if supported {
                let fallback = ["low", "medium", "high", "xhigh", "max"].map(Value::from);
                let levels = info
                    .and_then(|m| m["supportedEffortLevels"].as_array())
                    .filter(|levels| !levels.is_empty())
                    .map_or(fallback.as_slice(), |levels| levels.as_slice());
                for level in
                    std::iter::once(None).chain(levels.iter().filter_map(Value::as_str).map(Some))
                {
                    catalog.efforts.push(OptionRow {
                        section: "Effort",
                        label: effort_name(level).into(),
                        description: if level.is_none() {
                            "the model's default".into()
                        } else {
                            String::new()
                        },
                        change: SettingChange::Effort(level.map(str::to_string)),
                        selected: effort == level,
                        disabled: false,
                    });
                }
            }
            if !acp {
                let variant = variant(current);
                for (label, value, disabled) in [
                    ("200k", base(current).to_string(), false),
                    (
                        "1M",
                        variant.clone().unwrap_or_else(|| current.into()),
                        !wide && variant.is_none(),
                    ),
                ] {
                    catalog.efforts.push(OptionRow {
                        section: "Context window",
                        label: label.into(),
                        description: if disabled {
                            if listed {
                                "not offered for this model"
                            } else {
                                "after the session starts"
                            }
                            .into()
                        } else {
                            String::new()
                        },
                        change: SettingChange::Model(value),
                        selected: (label == "1M") == wide,
                        disabled,
                    });
                }
            }
        }
        if let Some(current) = state.acknowledged.mode.as_deref() {
            let mut modes = if acp {
                ready.map(|(_, modes)| modes.clone()).unwrap_or_default()
            } else {
                ["default", "acceptEdits", "plan", "auto"]
                    .map(|value| json!({"value":value, "displayName":mode_name(value)}))
                    .into()
            };
            // Iced retains acknowledged modes outside the normal cycle at its end.
            for event in &state.conversation {
                if let AgentEvent::Harness(HarnessEvent::PermissionModeChanged(value)) = event
                    && !modes.iter().any(|m| m["value"] == *value)
                {
                    modes.push(json!({"value":value, "displayName":if acp {value.as_str()} else {mode_name(value)}}));
                }
            }
            if !modes.iter().any(|m| m["value"] == current) {
                modes.push(json!({"value":current, "displayName":if acp {current} else {mode_name(current)}}));
            }
            catalog.modes = modes
                .iter()
                .filter_map(|info| {
                    let value = info["value"].as_str()?;
                    Some(OptionRow {
                        section: "Permission mode · Shift+Tab cycles",
                        label: info["displayName"].as_str().unwrap_or(value).into(),
                        description: info["description"].as_str().unwrap_or_default().into(),
                        change: SettingChange::Mode(value.into()),
                        selected: current == value,
                        disabled: false,
                    })
                })
                .collect();
        }
        catalog
    }
    pub fn cycle_mode(&self, pending: Option<&SettingChange>) -> Option<SettingChange> {
        if self.modes.is_empty() {
            return None;
        }
        let from = pending
            .and_then(|pending| self.modes.iter().position(|row| &row.change == pending))
            .or_else(|| self.modes.iter().position(|row| row.selected));
        Some(
            self.modes[from.map_or(0, |from| (from + 1) % self.modes.len())]
                .change
                .clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm_core::chat::state::ConfirmedEffort;

    pub fn fixture() -> State {
        let mut state = State::default();
        state.acknowledged.model = Some("opus".into());
        state.acknowledged.mode = Some("default".into());
        state.acknowledged.effort = Some(ConfirmedEffort {
            effort: Some("high".into()),
            applied: Some("high".into()),
        });
        state.conversation.push(AgentEvent::Harness(HarnessEvent::Ready {
            session_id: None, permission_mode: Some("default".into()),
            models: vec![
                json!({"value":"sonnet", "displayName":"Sonnet 5", "description":"Fast", "supportsEffort":true, "supportedEffortLevels":["medium","low"]}),
                json!({"value":"opus", "displayName":"Opus 5.5", "description":"Reasoning", "supportsEffort":true, "supportedEffortLevels":["low","high","xhigh","max"]}),
                json!({"value":"opus[1m]", "displayName":"Opus 1M", "supportsEffort":true, "supportedEffortLevels":["low","high","xhigh","max"]}),
                json!({"value":"sonnet[1m]", "supportsEffort":true}),
                json!({"value":"haiku", "displayName":"Haiku"}),
                json!({"missing":"value"}),
            ], modes: vec![json!({"value":"build", "displayName":"Build"}), json!({"value":"plan", "displayName":"Planning"})], commands: vec![],
        }));
        state
    }
    #[test]
    fn fixture_catalog_preserves_iced_options_labels_and_order() {
        let mut state = fixture();
        let catalog = Catalog::project(&state, false);
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Sonnet 5", "Opus 5.5", "Haiku"]
        );
        assert_eq!(catalog.models[0].description, "Fast");
        assert_eq!(
            catalog
                .efforts
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Auto", "Low", "High", "X-High", "Max", "200k", "1M"]
        );
        assert_eq!(catalog.efforts[0].description, "the model's default");
        assert_eq!(
            catalog.efforts[6].change,
            SettingChange::Model("opus[1m]".into())
        );
        assert_eq!(
            catalog
                .modes
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Default", "Accept edits", "Plan mode", "Auto mode"]
        );
        state.acknowledged.model = Some("opus[1m]".into());
        let catalog = Catalog::project(&state, false);
        assert_eq!(
            catalog.models[0].change,
            SettingChange::Model("sonnet[1m]".into())
        );
        assert_eq!(
            catalog.efforts[5].change,
            SettingChange::Model("opus".into())
        );
        assert!(catalog.efforts[6].selected);
        state.acknowledged.model = Some("haiku".into());
        let catalog = Catalog::project(&state, false);
        assert_eq!(
            catalog
                .efforts
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["200k", "1M"]
        );
        assert!(catalog.efforts[1].disabled);
        assert!(!catalog.offers(&SettingChange::Effort(Some("high".into()))));
    }
    #[test]
    fn acp_uses_provider_modes_efforts_and_no_context_choices() {
        let mut state = fixture();
        state.acknowledged.mode = Some("build".into());
        state.acknowledged.model = Some("sonnet".into());
        let catalog = Catalog::project(&state, true);
        assert_eq!(
            catalog
                .modes
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Build", "Planning"]
        );
        assert_eq!(
            catalog
                .efforts
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Auto", "Medium", "Low"]
        );
        state.acknowledged.model = Some("haiku".into());
        assert!(Catalog::project(&state, true).efforts.is_empty());
        assert!(Catalog::project(&State::default(), true).models.is_empty());
    }
    #[test]
    fn cycle_mode_uses_iced_order_and_retains_extra_acknowledged_modes() {
        let mut state = fixture();
        for (current, next) in [
            ("default", "acceptEdits"),
            ("acceptEdits", "plan"),
            ("plan", "auto"),
            ("auto", "default"),
        ] {
            state.acknowledged.mode = Some(current.into());
            assert_eq!(
                Catalog::project(&state, false).cycle_mode(None),
                Some(SettingChange::Mode(next.into()))
            );
        }
        state.acknowledged.mode = Some("bypassPermissions".into());
        state
            .conversation
            .push(AgentEvent::Harness(HarnessEvent::PermissionModeChanged(
                "bypassPermissions".into(),
            )));
        let catalog = Catalog::project(&state, false);
        assert_eq!(catalog.modes.last().unwrap().label, "Bypass permissions");
        assert_eq!(
            catalog.cycle_mode(None),
            Some(SettingChange::Mode("default".into()))
        );
        assert_eq!(
            catalog.cycle_mode(Some(&SettingChange::Mode("auto".into()))),
            Some(SettingChange::Mode("bypassPermissions".into()))
        );
        assert!(Catalog::default().cycle_mode(None).is_none());
    }
}
