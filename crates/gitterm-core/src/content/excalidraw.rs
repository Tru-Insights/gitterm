// Excalidraw integration module — read-only preview (Phase 1)
//
// Renders .excalidraw files in the file viewer WebView using the
// Excalidraw React component loaded from CDN.

use std::path::Path;

/// Check if a file path has an `.excalidraw` extension.
pub fn is_excalidraw_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("excalidraw"))
        .unwrap_or(false)
}

/// Basic validation: must be valid JSON with `"type": "excalidraw"`.
pub fn validate_excalidraw(json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    value
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "excalidraw")
        .unwrap_or(false)
}

/// Generate a self-contained HTML page that renders the Excalidraw data
/// in read-only mode via the Excalidraw CDN bundle.
pub fn render_excalidraw_html(json_content: &str, is_dark: bool) -> String {
    let theme = if is_dark { "dark" } else { "light" };

    // The JSON is embedded in a <script type="application/json"> tag,
    // so the only thing we need to escape is </script> sequences.
    let safe_json = json_content.replace("</script>", "<\\/script>");

    let template = include_str!("../../../../assets/excalidraw/viewer.html");
    template
        .replace("{{EXCALIDRAW_DATA}}", &safe_json)
        .replace("{{THEME}}", theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // === is_excalidraw_file ===

    #[test]
    fn detects_excalidraw_extension() {
        assert!(is_excalidraw_file(Path::new("diagram.excalidraw")));
    }

    #[test]
    fn detects_excalidraw_case_insensitive() {
        assert!(is_excalidraw_file(Path::new("DIAGRAM.Excalidraw")));
    }

    #[test]
    fn rejects_non_excalidraw_extension() {
        assert!(!is_excalidraw_file(Path::new("diagram.json")));
        assert!(!is_excalidraw_file(Path::new("diagram.png")));
        assert!(!is_excalidraw_file(Path::new("main.rs")));
    }

    #[test]
    fn rejects_no_extension() {
        assert!(!is_excalidraw_file(Path::new("excalidraw")));
    }

    // === validate_excalidraw ===

    #[test]
    fn validates_correct_excalidraw_json() {
        let json = r#"{"type":"excalidraw","version":2,"elements":[]}"#;
        assert!(validate_excalidraw(json));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(!validate_excalidraw("not json at all"));
    }

    #[test]
    fn rejects_json_without_type() {
        assert!(!validate_excalidraw(r#"{"version":2,"elements":[]}"#));
    }

    #[test]
    fn rejects_json_with_wrong_type() {
        assert!(!validate_excalidraw(r#"{"type":"other","elements":[]}"#));
    }

    // === render_excalidraw_html ===

    #[test]
    fn html_contains_dark_theme() {
        let json = r#"{"type":"excalidraw","version":2,"elements":[]}"#;
        let html = render_excalidraw_html(json, true);
        assert!(html.contains("\"dark\""));
        assert!(!html.contains("\"light\""));
    }

    #[test]
    fn html_contains_light_theme() {
        let json = r#"{"type":"excalidraw","version":2,"elements":[]}"#;
        let html = render_excalidraw_html(json, false);
        assert!(html.contains("\"light\""));
    }

    #[test]
    fn html_contains_excalidraw_data() {
        let json = r#"{"type":"excalidraw","version":2,"elements":[]}"#;
        let html = render_excalidraw_html(json, true);
        assert!(html.contains("excalidraw"));
        assert!(html.contains("elements"));
    }

    #[test]
    fn html_contains_react_cdn() {
        let json = r#"{"type":"excalidraw","version":2,"elements":[]}"#;
        let html = render_excalidraw_html(json, true);
        assert!(html.contains("react"));
    }
}
