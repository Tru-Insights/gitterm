//! The plain PR/issue/merge-plan formats must work without runtime features.
use gitterm_core::{issues::IssueTitlesFile, merge_plan::MergePlan, prs::WatchFile};
use serde_json::Value;

#[test]
fn plain_formats_match_pre_extraction_bytes() {
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage45");
    let input: Value = serde_json::from_slice(
        &std::fs::read(fixtures.join("identity/formats.input.json")).unwrap(),
    )
    .unwrap();
    for label in ["old", "current"] {
        let case = &input[label];
        let watch: WatchFile = serde_json::from_value(case["watch"].clone()).unwrap();
        let issues: IssueTitlesFile = serde_json::from_value(case["issues"].clone()).unwrap();
        let plan: MergePlan = serde_json::from_value(case["merge_plan"].clone()).unwrap();
        for (name, bytes) in [
            ("watch", serde_json::to_vec_pretty(&watch).unwrap()),
            ("issues", serde_json::to_vec_pretty(&issues).unwrap()),
            ("merge-plan", serde_json::to_vec_pretty(&plan).unwrap()),
        ] {
            assert_eq!(
                bytes,
                std::fs::read(fixtures.join(format!("pr-services/{label}.{name}.json"))).unwrap()
            );
        }
    }
}
