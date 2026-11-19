use serde_json::Value;
use std::path::Path;

pub fn golden(name: &str, actual: Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native-chat")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read golden {}: {error}", path.display()));
    let expected: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse golden {}: {error}", path.display()));
    assert_eq!(actual, expected, "golden {}", path.display());
}
