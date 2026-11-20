//! The CLI's account schema is deliberately not inferred from documentation.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "production table intentionally empty until coordinator observes CLI schema"
    )
)]
pub(super) enum Field {
    SignedIn,
    Method,
    Subscription,
    ConfigDirectory,
    /// Every organization, team, enterprise, API/Console or third-party indicator
    /// in the observed schema must be listed and have the verified empty value.
    PersonalOnly(EmptyIndicator),
    /// Verified display-only strings (e.g. account labels), never policy fields.
    InformationalString,
}
#[derive(Clone, Copy)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "production table is empty until coordinator observes the schema"
    )
)]
pub(super) enum EmptyIndicator {
    Null,
    False,
    EmptyString,
}
impl EmptyIndicator {
    fn matches(self, value: &Value) -> bool {
        match self {
            Self::Null => value.is_null(),
            Self::False => value.as_bool() == Some(false),
            Self::EmptyString => value.as_str() == Some(""),
        }
    }
}
#[derive(Clone, Copy)]
pub(super) struct Schema {
    pub unverified: bool,
    pub fields: &'static [(&'static str, Field)],
    pub method: &'static str,
    pub subscriptions: &'static [&'static str],
}
// UNVERIFIED until coordinator probe. No field names or account values guessed.
// Coordinator: populate ALL observed keys/values (including negative indicators)
// from isolated `claude auth status --json`, update fixtures, then set false.
// Unknown fields fail closed, so new policy/account indicators cannot slip through.
pub(super) const AUTH_STATUS_SCHEMA: Schema = Schema {
    unverified: true,
    fields: &[],
    method: "",
    subscriptions: &[],
};

pub(super) fn personal_auth(
    exit: Option<i32>,
    bytes: &[u8],
    root: &Path,
    schema: Schema,
) -> Result<()> {
    ensure!(
        !schema.unverified,
        "Claude preview auth schema is UNVERIFIED until coordinator probe (chat/auth.rs AUTH_STATUS_SCHEMA); launch refused"
    );
    ensure!(
        exit == Some(0),
        "Claude preview requires a signed-in personal Pro or Max subscription; auth status failed or signed out"
    );
    ensure!(bytes.len() <= 16 * 1024, "Claude auth status exceeds limit");
    let value: Value = serde_json::from_slice(bytes).context("unparseable Claude auth status")?;
    let object = value
        .as_object()
        .context("Claude auth status must be an object")?;
    ensure!(
        object.len() == schema.fields.len(),
        "unknown or missing Claude auth fields; personal account cannot be established"
    );
    let mut required = [false; 4];
    for (name, field) in schema.fields {
        let value = object
            .get(*name)
            .context("missing required Claude auth field")?;
        match field {
            Field::SignedIn => {
                ensure!(
                    value.as_bool() == Some(true),
                    "Claude preview is signed out"
                );
                required[0] = true;
            }
            Field::Method => {
                ensure!(
                    !schema.method.is_empty() && value.as_str() == Some(schema.method),
                    "Claude preview refuses API keys, Console and third-party authentication"
                );
                required[1] = true;
            }
            Field::Subscription => {
                ensure!(
                    value
                        .as_str()
                        .is_some_and(|s| schema.subscriptions.contains(&s)),
                    "Claude preview requires personal Pro or Max; Team/Enterprise and other plans are refused"
                );
                required[2] = true;
            }
            Field::ConfigDirectory => {
                ensure!(
                    value
                        .as_str()
                        .is_some_and(|value| Some(value) == root.to_str()),
                    "Claude auth status provider root mismatch"
                );
                required[3] = true;
            }
            Field::InformationalString => {
                ensure!(value.is_string(), "invalid Claude account display field")
            }
            Field::PersonalOnly(empty) => ensure!(
                empty.matches(value),
                "Claude preview refuses organization/team/enterprise, API/Console or third-party indicators"
            ),
        }
    }
    ensure!(
        required.into_iter().all(|v| v),
        "incomplete verified Claude auth schema"
    );
    Ok(())
}

#[cfg(test)]
pub(super) const FIXTURE_SCHEMA: Schema = Schema {
    unverified: false,
    fields: &[
        ("fixture_signed_in", Field::SignedIn),
        ("fixture_method", Field::Method),
        ("fixture_plan", Field::Subscription),
        ("fixture_root", Field::ConfigDirectory),
        ("fixture_policy", Field::PersonalOnly(EmptyIndicator::Null)),
    ],
    method: "fixture_personal",
    subscriptions: &["fixture_pro", "fixture_max"],
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_personal_schema_passes_and_unverified_never_does() {
        let root = Path::new("/isolated/providers/claude");
        let good = serde_json::json!({"fixture_signed_in":true,"fixture_method":"fixture_personal","fixture_plan":"fixture_pro","fixture_root":root,"fixture_policy":null});
        let check = |v: &Value| {
            personal_auth(
                Some(0),
                &serde_json::to_vec(v).unwrap(),
                root,
                FIXTURE_SCHEMA,
            )
        };
        check(&good).unwrap();
        assert!(
            personal_auth(
                Some(0),
                &serde_json::to_vec(&good).unwrap(),
                root,
                AUTH_STATUS_SCHEMA
            )
            .is_err()
        );
        for (key, bad) in [
            ("fixture_signed_in", serde_json::json!(false)),
            ("fixture_method", serde_json::json!("api_key")),
            ("fixture_method", serde_json::json!("console")),
            ("fixture_method", serde_json::json!("third_party")),
            ("fixture_plan", serde_json::json!("team")),
            ("fixture_plan", serde_json::json!("enterprise")),
            ("fixture_policy", serde_json::json!("organization")),
            ("fixture_root", serde_json::json!("/wrong")),
            ("unknown_org", serde_json::json!(true)),
        ] {
            let mut invalid = good.clone();
            invalid[key] = bad;
            assert!(check(&invalid).is_err(), "{key}");
        }
        let display_schema = Schema {
            fields: &[
                ("fixture_signed_in", Field::SignedIn),
                ("fixture_method", Field::Method),
                ("fixture_plan", Field::Subscription),
                ("fixture_root", Field::ConfigDirectory),
                ("fixture_policy", Field::PersonalOnly(EmptyIndicator::Null)),
                ("fixture_display", Field::InformationalString),
            ],
            ..FIXTURE_SCHEMA
        };
        let mut display = good.clone();
        display["fixture_display"] = serde_json::json!("Synthetic account label");
        personal_auth(
            Some(0),
            &serde_json::to_vec(&display).unwrap(),
            root,
            display_schema,
        )
        .unwrap();
        display["fixture_display"] = serde_json::json!({"organization": "team"});
        assert!(
            personal_auth(
                Some(0),
                &serde_json::to_vec(&display).unwrap(),
                root,
                display_schema
            )
            .is_err()
        );
        assert!(EmptyIndicator::False.matches(&serde_json::json!(false)));
        assert!(EmptyIndicator::EmptyString.matches(&serde_json::json!("")));
        assert!(!EmptyIndicator::Null.matches(&serde_json::json!("organization")));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid_root =
                std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
            let mut invalid = good.clone();
            invalid["fixture_root"] = Value::Null;
            assert!(
                personal_auth(
                    Some(0),
                    &serde_json::to_vec(&invalid).unwrap(),
                    &invalid_root,
                    FIXTURE_SCHEMA
                )
                .is_err()
            );
        }
        for bytes in [b"garbage".as_slice(), b"[]", b"{}"] {
            assert!(personal_auth(Some(0), bytes, root, FIXTURE_SCHEMA).is_err());
        }
        assert!(
            personal_auth(
                Some(1),
                &serde_json::to_vec(&good).unwrap(),
                root,
                FIXTURE_SCHEMA
            )
            .is_err()
        );
    }
}
