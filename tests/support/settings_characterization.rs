//! Captured against the root serializer before the Stage 5 settings move.
use super::*;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stage45/settings")
            .join(name),
    )
    .unwrap()
}

fn expected(name: &str) -> String {
    let suffix = if cfg!(feature = "stt") { "" } else { ".no-stt" };
    fixture(&format!("{name}{suffix}.json"))
}

#[test]
fn pinned_settings_load_save_bytes() {
    for name in ["full", "legacy", "unknown", "missing"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        std::fs::write(&path, fixture(&format!("{name}.input.json"))).unwrap();
        let loaded = load_settings_from(dir.path());
        assert!(loaded.refused.is_none(), "{name}: {:?}", loaded.refused);
        save_settings_to(dir.path(), &loaded.config).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, expected(name), "{name}");
        let reloaded = load_settings_from(dir.path());
        save_settings_to(dir.path(), &reloaded.config).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), saved, "{name}");
    }
}

#[test]
fn pinned_settings_default_constructor_bytes() {
    let dir = tempfile::tempdir().unwrap();
    save_settings_to(dir.path(), &Config::default()).unwrap();
    let saved = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
    assert_eq!(saved, expected("default"));
    // These intentionally differ from deserializing an empty object.
    let missing: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(missing.scrollback_lines, 100_000);
    assert!(!missing.show_hidden);
    assert_eq!(Config::default().scrollback_lines, 30_000);
    assert!(Config::default().show_hidden);
}

#[test]
fn pinned_settings_malformed_and_refused_values_preserve_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SETTINGS_FILE);
    let mut inputs: Vec<String> = serde_json::from_str(&fixture("refused.input.json")).unwrap();
    inputs.push(fixture("malformed.input.json"));
    #[cfg(feature = "stt")]
    inputs.extend([
        r#"{"stt_enabled":null}"#.into(),
        r#"{"stt_enabled":true,"stt_enabled":false}"#.into(),
    ]);
    for input in inputs {
        std::fs::write(&path, &input).unwrap();
        let loaded = load_settings_from(dir.path());
        let reason = loaded.refused.expect("invalid settings must be refused");
        assert!(reason.contains("malformed"), "{reason}");
        assert!(reason.contains("not writing"), "{reason}");
        assert_eq!(loaded.config.editor, Config::default().editor);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), input);
    }
}

#[test]
fn pinned_settings_legacy_migration_and_unknown_keys() {
    let legacy: Config = serde_json::from_str(&fixture("legacy.input.json")).unwrap();
    assert_eq!(legacy.font_size, Some(19.0));
    assert_eq!(legacy.chat_font_size, MAX_CHAT_FONT_SIZE);
    assert_eq!(legacy.agent_presets[0].command, "agy");
    assert_eq!(legacy.agent_presets[1].command, "gemini --custom");
    assert_eq!(legacy.agent_presets[1].color, WorkspaceColor::Lavender);
    let below: Config = serde_json::from_str(r#"{"chat_font_size":1}"#).unwrap();
    assert_eq!(below.chat_font_size, MIN_CHAT_FONT_SIZE);
    let unknown: Config = serde_json::from_str(&fixture("unknown.input.json")).unwrap();
    let value = serde_json::to_value(unknown).unwrap();
    assert!(value.get("unknown_setting").is_none());
    assert!(value["chat"].get("unknown_chat").is_none());
}

#[test]
fn pinned_settings_empty_sequence_uses_field_defaults() {
    // Serde accepts an empty sequence for this all-defaulted struct too.
    let parsed: Config = serde_json::from_str("[]").unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&parsed).unwrap(),
        expected("missing")
    );
}

#[test]
fn pinned_settings_ignored_unknown_values_and_disabled_voice() {
    let huge: Config = serde_json::from_str(r#"{"future":1e400}"#).unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&huge).unwrap(),
        expected("missing")
    );
    #[cfg(not(feature = "stt"))]
    {
        let ignored: Config = serde_json::from_str(
            r#"{"stt_enabled":null,"stt_enabled":"future","stt_model_path":17}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&ignored).unwrap(),
            expected("missing")
        );
    }
}

#[test]
fn pinned_settings_legacy_carryover_uses_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let instance = dir.path().join("instance-synthetic");
    std::fs::create_dir_all(&instance).unwrap();
    std::fs::write(
        instance.join(INSTANCE_CONFIG_FILE),
        fixture("legacy.input.json"),
    )
    .unwrap();
    let loaded = load_settings_from(dir.path());
    assert!(loaded.refused.is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(),
        expected("legacy")
    );
}

#[test]
fn pinned_settings_populated_sequence_keeps_field_positions() {
    let dir = tempfile::tempdir().unwrap();
    let suffix = if cfg!(feature = "stt") { "" } else { ".no-stt" };
    std::fs::write(
        dir.path().join(SETTINGS_FILE),
        fixture(&format!("full-sequence{suffix}.input.json")),
    )
    .unwrap();
    let loaded = load_settings_from(dir.path());
    assert!(loaded.refused.is_none(), "{:?}", loaded.refused);
    save_settings_to(dir.path(), &loaded.config).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(),
        expected("full")
    );
}
