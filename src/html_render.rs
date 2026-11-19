//! Compatibility facade for retained HTML contracts and native IPC validation.
pub use gitterm_core::content::html_render::*;

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_harness_event_still_deserializes() {
        let legacy = serde_json::json!({"type":"text_delta","data":"old conversation"});
        assert!(serde_json::from_value::<crate::harness::HarnessEvent>(legacy).is_ok());
    }
}
