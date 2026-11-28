//! Iced's page navigation vectors (src/main.rs tests), run against the
//! shared decision.
use super::*;

#[test]
fn page_new_windows_and_iframes_match_iced_http_only_policy() {
    for url in [
        "http://example.com/frame",
        "https://github.com/example/repo/pull/1",
    ] {
        assert_eq!(
            page_new_window_decision(url),
            PageNavigationDecision::External
        );
    }
    for url in [
        "not a URL",
        "file:///tmp/fixture",
        "about:blank",
        "javascript:alert(1)",
        "mailto:person@example.com",
    ] {
        assert_eq!(
            page_new_window_decision(url),
            PageNavigationDecision::Cancel
        );
    }
}

fn page_context() -> PageNavigationContext {
    PageNavigationContext {
        url: "https://github.com/sample-owner-a/sample-repo/pull/1".to_string(),
        store: PageStore::GitHub("sample-login-a".to_string()),
        github_accounts: [
            ("sample-owner-a".to_string(), "sample-login-a".to_string()),
            ("sample-alias-a".to_string(), "SAMPLE-LOGIN-A".to_string()),
            ("sample-owner-b".to_string(), "sample-login-b".to_string()),
        ]
        .into(),
        linear_workspaces: [
            "sample-workspace-a".to_string(),
            "sample-workspace-b".to_string(),
        ]
        .into(),
    }
}

#[test]
fn github_social_sign_in_keeps_the_current_store_across_redirects() {
    use PageNavigationDecision::Allow;
    let providers = [
        "https://accounts.google.com/v3/signin/identifier",
        "https://appleid.apple.com/auth/authorize",
        "https://idmsa.apple.com/appleauth/auth/authorize",
    ];
    for store in [
        PageStore::GitHub("acme-login".to_string()),
        PageStore::GitHub("other-login".to_string()),
        PageStore::Default,
    ] {
        let mut context = page_context();
        context.store = store;
        for origin in [
            "https://github.com/login",
            "https://github.com/session",
            "https://github.com/sessions/two-factor/app",
        ] {
            context.url = origin.to_string();
            for destination in providers {
                assert_eq!(page_navigation_decision(destination, &context), Allow);
            }
        }
        for origin in providers {
            context.url = origin.to_string();
            for destination in providers.into_iter().chain([
                "https://github.com/login",
                "https://github.com/session",
                "https://github.com/sessions/social/google/callback?code=x",
                "https://github.com/sessions/social/apple/callback?code=x",
            ]) {
                assert_eq!(page_navigation_decision(destination, &context), Allow);
            }
        }
    }
}

#[test]
fn social_sign_in_exception_requires_a_trusted_sign_in_origin_and_destination() {
    use PageNavigationDecision::{External, Reopen};
    let mut context = page_context();
    for origin in [
        "https://github.com/acme/api/pull/1",
        "https://github.com/",
        "https://github.com/settings",
        "https://github.com/logout",
        "https://github.com/login-extra",
        "https://github.com/%6cogin",
        "https://github.com//login",
        "https://linear.app/login",
        "https://example.com/login",
        "http://github.com/login",
        "https://github.com:8443/login",
        "https://user:password@github.com/login",
        "http://accounts.google.com/",
        "https://accounts.google.com:8443/",
        "https://user:password@accounts.google.com/",
        "https://accounts.google.com.example.com/",
        "malformed",
    ] {
        context.url = origin.to_string();
        for destination in [
            "https://accounts.google.com/",
            "https://appleid.apple.com/",
            "https://idmsa.apple.com/",
        ] {
            assert_eq!(
                page_navigation_decision(destination, &context),
                External,
                "{origin}"
            );
        }
    }
    for origin in ["https://github.com/login", "https://accounts.google.com/"] {
        context.url = origin.to_string();
        for destination in [
            "https://example.com/",
            "https://accounts.google.com.example.com/",
            "https://accounts.google.com./",
            "https://sub.accounts.google.com/",
            "http://accounts.google.com/",
            "https://accounts.google.com:8443/",
            "https://user:password@accounts.google.com/",
            "https://appleid.apple.com.example.com/",
        ] {
            assert_eq!(page_navigation_decision(destination, &context), External);
        }
    }
    context.url = "https://accounts.google.com/".to_string();
    for destination in ["https://github.com/", "https://github.com/settings"] {
        assert_eq!(
            page_navigation_decision(destination, &context),
            Reopen(PageStore::Default)
        );
    }
}

#[test]
fn page_navigation_preserves_same_identity_and_reopens_changed_identities() {
    use PageNavigationDecision::{Allow, Reopen};
    let context = page_context();
    for url in [
        "https://github.com/sample-owner-a/sample-repo/pull/2",
        "https://github.com/sample-alias-a/sample-repo",
        "https://github.com/login?return_to=%2Fsample-owner-a",
        "https://github.com/login/oauth/authorize",
        "https://github.com/session",
        "https://github.com/sessions/two-factor/app",
        "https://github.com/settings/profile",
        "https://github.com/logout",
    ] {
        assert_eq!(page_navigation_decision(url, &context), Allow, "{url}");
    }
    assert_eq!(
        page_navigation_decision("https://github.com/sample-owner-b/sample-repo", &context),
        Reopen(PageStore::GitHub("sample-login-b".to_string())),
    );
    assert_eq!(
        page_navigation_decision(
            "https://github.com/unmapped-owner/sample-repo/pull/1",
            &context
        ),
        Reopen(PageStore::Default),
    );
    assert_eq!(
        page_navigation_decision(
            "https://linear.app/sample-workspace-a/issue/ABC-12/example",
            &context
        ),
        Reopen(PageStore::Linear("sample-workspace-a".to_string())),
    );
}

#[test]
fn linear_navigation_uses_canonical_or_known_workspaces_and_preserves_login() {
    use PageNavigationDecision::{Allow, External, Reopen};
    let mut context = page_context();
    context.url = "https://linear.app/sample-workspace-a/issue/ABC-12/example".to_string();
    context.store = PageStore::Linear("sample-workspace-a".to_string());
    for url in [
        "https://linear.app/sample-workspace-a/issue/ABC-13/example",
        "https://linear.app/sample-workspace-a/team/example",
        "https://linear.app/login",
        "https://linear.app/oauth/authorize?client_id=sample",
    ] {
        assert_eq!(page_navigation_decision(url, &context), Allow, "{url}");
    }
    for url in [
        "https://linear.app/sample-workspace-b/issue/ABC-14/example",
        "https://linear.app/sample-workspace-b/team/example",
    ] {
        assert_eq!(
            page_navigation_decision(url, &context),
            Reopen(PageStore::Linear("sample-workspace-b".to_string())),
            "{url}"
        );
    }
    assert_eq!(
        page_navigation_decision(
            "https://linear.app/unknown-workspace/team/example",
            &context
        ),
        External
    );
}

#[test]
fn page_navigation_routes_foreign_urls_externally_and_cancels_forbidden_schemes() {
    use PageNavigationDecision::{Cancel, External};
    let context = page_context();
    for url in [
        "https://example.test/",
        "http://example.test/",
        "http://github.com/login",
        "http://github.com/session",
        "http://linear.app/login",
        "https://github.com.example.test/sample-owner-b/repo/pull/1",
        "https://user@github.com/sample-owner-b/repo/pull/1",
        "https://github.com:8443/sample-owner-b/repo/pull/1",
        "https://github.com/explore",
        "https://github.com/%73ample-owner-b/repo/pull/1",
        "https://github.com//repo/pull/1",
        "https://linear.app/%73ample-workspace-b/issue/ABC-12/example",
    ] {
        assert_eq!(page_navigation_decision(url, &context), External, "{url}");
    }
    for url in [
        "file:///tmp/example",
        "javascript:alert(1)",
        "data:text/plain,example",
        "sample-app:open",
        "malformed",
    ] {
        assert_eq!(page_navigation_decision(url, &context), Cancel, "{url}");
    }
}
#[test]
fn newly_known_linear_workspaces_select_their_store() {
    let mut context = page_context();
    context.url = "https://linear.app/acme/issue/ABC-1/example".to_string();
    context.store = PageStore::Linear("acme".to_string());
    let destination = "https://linear.app/new-workspace/team/example";
    assert_eq!(
        page_navigation_decision(destination, &context),
        PageNavigationDecision::External
    );
    context
        .linear_workspaces
        .insert("new-workspace".to_string());
    assert_eq!(
        page_navigation_decision(destination, &context),
        PageNavigationDecision::Reopen(PageStore::Linear("new-workspace".to_string()))
    );
}

#[test]
fn an_allowed_destination_becomes_the_origin_for_the_next() {
    let mut context = page_context();
    context.store = PageStore::Default;
    context.url = "https://github.com/unmapped-owner-a/sample-repo/pull/1".to_string();
    let next = "https://github.com/unmapped-owner-b/sample-repo/pull/1";
    let after = "https://github.com/unmapped-owner-b/sample-repo";
    assert_eq!(
        page_navigation_decision(next, &context),
        PageNavigationDecision::Allow
    );
    assert_eq!(
        page_navigation_decision(after, &context),
        PageNavigationDecision::External
    );
    context.url = next.to_string();
    assert_eq!(
        page_navigation_decision(after, &context),
        PageNavigationDecision::Allow
    );
}

#[test]
fn store_comparison_normalizes_github_login_case() {
    let mut context = page_context();
    context.store = PageStore::GitHub("Sample-Login-A".to_string());
    // Iced compares the normalized destination with the host's store as given.
    assert_eq!(
        page_navigation_decision("https://github.com/sample-owner-a/sample-repo", &context),
        PageNavigationDecision::Reopen(PageStore::GitHub("sample-login-a".to_string()))
    );
}
