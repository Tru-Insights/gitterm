//! The CLI's account schema is deliberately not inferred from documentation.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum Field {
    SignedIn,
    Method,
    Provider,
    Subscription,
    ConfigDirectory,
    /// Typed display-only data, never account-policy evidence.
    InformationalString,
    InformationalBool,
}
#[derive(Clone, Copy)]
pub(super) struct Schema {
    pub unverified: bool,
    pub fields: &'static [(&'static str, Field)],
    pub method: &'static str,
    pub provider: &'static str,
    pub subscriptions: &'static [&'static str],
}
// Verified by the coordinator's isolated Claude Code 2.1.327 probe.
// Personal accounts have org fields: only subscriptionType establishes the plan.
// Unknown or missing fields fail closed; never include returned values in errors.
pub(super) const AUTH_STATUS_SCHEMA: Schema = Schema {
    unverified: false,
    fields: &[
        ("loggedIn", Field::SignedIn),
        ("authMethod", Field::Method),
        ("apiProvider", Field::Provider),
        ("subscriptionType", Field::Subscription),
        ("configDirectory", Field::ConfigDirectory),
        ("planTier", Field::InformationalString),
        ("storedClaudeAiLogin", Field::InformationalBool),
        ("projectsDirectory", Field::InformationalString),
        ("loginExpiresAt", Field::InformationalString),
        ("analyticsDisabled", Field::InformationalBool),
        ("consoleProfileSignInOffered", Field::InformationalBool),
        ("email", Field::InformationalString),
        ("orgId", Field::InformationalString),
        ("orgName", Field::InformationalString),
    ],
    method: "claude.ai",
    provider: "firstParty",
    subscriptions: &["pro", "max"],
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
    let mut required = [false; 5];
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
            Field::Provider => {
                ensure!(
                    !schema.provider.is_empty() && value.as_str() == Some(schema.provider),
                    "Claude preview requires first-party authentication"
                );
                required[4] = true;
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
            Field::InformationalBool => {
                ensure!(value.is_boolean(), "invalid Claude account boolean field")
            }
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
        ("fixture_provider", Field::Provider),
    ],
    method: "fixture_personal",
    provider: "fixture_first_party",
    subscriptions: &["fixture_pro", "fixture_max"],
};
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[test]
    fn only_explicit_personal_schema_passes_and_unverified_never_does() {
        let root = Path::new("/isolated/providers/claude");
        let good = serde_json::json!({"fixture_signed_in":true,"fixture_method":"fixture_personal","fixture_plan":"fixture_pro","fixture_root":root,"fixture_provider":"fixture_first_party"});
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
                Schema {
                    unverified: true,
                    ..FIXTURE_SCHEMA
                }
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
            ("fixture_provider", serde_json::json!("third_party")),
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
                ("fixture_provider", Field::Provider),
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

    pub(in crate::chat) fn signed_in(root: &Path) -> Value {
        let mut value: Value =
            serde_json::from_str(include_str!("fixtures/auth-max.json")).unwrap();
        value["configDirectory"] = serde_json::json!(root);
        value["projectsDirectory"] = serde_json::json!(root.join("projects"));
        value
    }

    #[test]
    fn verified_personal_pro_and_max_pass_with_org_fields() {
        let root = Path::new("/isolated/providers/claude");
        for plan in ["pro", "max"] {
            let mut value = signed_in(root);
            value["subscriptionType"] = serde_json::json!(plan);
            personal_auth(
                Some(0),
                &serde_json::to_vec(&value).unwrap(),
                root,
                AUTH_STATUS_SCHEMA,
            )
            .unwrap();
        }
        // Also exercise the public table's verification flag, independently of input.
        let schema = std::hint::black_box(AUTH_STATUS_SCHEMA);
        assert!(!schema.unverified);
    }

    #[test]
    fn verified_schema_refuses_nonpersonal_auth_and_shape_drift_without_values_in_errors() {
        let root = Path::new("/isolated/providers/claude");
        let good = signed_in(root);
        let check = |value: &Value| {
            personal_auth(
                Some(0),
                &serde_json::to_vec(value).unwrap(),
                root,
                AUTH_STATUS_SCHEMA,
            )
        };
        let signed_out: Value =
            serde_json::from_str(include_str!("fixtures/auth-signed-out.json")).unwrap();
        assert!(check(&signed_out).is_err());
        for (key, bad) in [
            ("loggedIn", serde_json::json!(false)),
            ("loggedIn", serde_json::json!("true")),
            ("subscriptionType", serde_json::json!("team")),
            ("subscriptionType", serde_json::json!("enterprise")),
            ("subscriptionType", serde_json::json!("unknown")),
            ("subscriptionType", Value::Null),
            ("authMethod", serde_json::json!("none")),
            ("authMethod", serde_json::json!("api_key")),
            ("authMethod", serde_json::json!("console")),
            ("authMethod", serde_json::json!("third_party")),
            ("apiProvider", serde_json::json!("thirdParty")),
            ("apiProvider", Value::Null),
            ("configDirectory", serde_json::json!("/wrong")),
        ] {
            let mut value = good.clone();
            value[key] = bad;
            assert!(check(&value).is_err(), "{key}");
        }
        for (key, field) in AUTH_STATUS_SCHEMA.fields {
            let mut value = good.clone();
            value.as_object_mut().unwrap().remove(*key);
            assert!(check(&value).is_err(), "missing {key}");
            let mut value = good.clone();
            value[key] = serde_json::json!({"synthetic-private-marker": true});
            let error = format!("{:#}", check(&value).unwrap_err());
            assert!(!error.contains("synthetic-private-marker"));
            for display in [
                "user@example.invalid",
                "00000000-0000-0000-0000-000000000000",
                "Example Org",
                "synthetic-tier",
            ] {
                assert!(!error.contains(display));
            }
            if matches!(field, Field::InformationalString) {
                let mut value = good.clone();
                value[key] = serde_json::json!("arbitrary synthetic display data");
                check(&value).unwrap();
            }
        }
        let mut value = good.clone();
        value["unknownPolicy"] = Value::Null;
        assert!(check(&value).is_err());
        // An unknown key replacing a known key must fail even with the same count.
        value.as_object_mut().unwrap().remove("email");
        assert!(check(&value).is_err());
        assert!(
            personal_auth(
                Some(1),
                &serde_json::to_vec(&good).unwrap(),
                root,
                AUTH_STATUS_SCHEMA
            )
            .is_err()
        );
        assert!(
            personal_auth(
                Some(0),
                &vec![b' '; 16 * 1024 + 1],
                root,
                AUTH_STATUS_SCHEMA
            )
            .is_err()
        );
    }
}
