//! Link policy for an embedded GitHub or Linear Page, shared by the Iced and
//! GPUI hosts so both route in-page navigation the same way.
use std::collections::{BTreeMap, BTreeSet};

use crate::page_store::{browser_url, PageStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageNavigationDecision {
    /// Load in the current Page and store.
    Allow,
    /// Cancel, then reopen the URL in the Page with this store.
    Reopen(PageStore),
    /// Cancel, then open the (HTTP(S)) URL in the default browser.
    External,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct PageNavigationContext {
    /// The URL the Page currently shows (the navigation's origin).
    pub url: String,
    pub store: PageStore,
    /// GitHub owner -> gh login, as `prs::account_for` reads it.
    pub github_accounts: BTreeMap<String, String>,
    /// Linear workspace slugs known from cached issues.
    pub linear_workspaces: BTreeSet<String>,
}

/// Iced's wry new-window hook also receives iframe navigations. Both open
/// HTTP(S) externally and cancel every other scheme.
pub fn page_new_window_decision(url: &str) -> PageNavigationDecision {
    if browser_url(url).is_ok() {
        PageNavigationDecision::External
    } else {
        PageNavigationDecision::Cancel
    }
}

fn github_sign_in_url(url: &url::Url) -> bool {
    url.host_str() == Some("github.com")
        && matches!(
            url.path_segments().and_then(|mut segments| segments.next()),
            Some("login" | "session" | "sessions")
        )
}

fn page_sign_in_provider(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("accounts.google.com" | "appleid.apple.com" | "idmsa.apple.com")
    )
}

/// Decide a main-frame navigation from `context.url` to `url`.
pub fn page_navigation_decision(
    url: &str,
    context: &PageNavigationContext,
) -> PageNavigationDecision {
    use PageNavigationDecision::{Allow, Cancel, External, Reopen};
    let Ok(destination) = browser_url(url) else {
        return Cancel;
    };
    if destination.scheme() != "https"
        || !destination.username().is_empty()
        || destination.password().is_some()
        || destination.port().is_some()
    {
        return External;
    }
    let origin = url::Url::parse(&context.url).ok();
    let same_service = origin.as_ref().and_then(url::Url::host_str) == destination.host_str();
    let segments: Vec<_> = destination.path_segments().into_iter().flatten().collect();
    let first = segments.first().copied().unwrap_or_default();
    // Encoded or empty identity prefixes are ambiguous; let the browser resolve them.
    if destination.path() != "/" && (first.is_empty() || first.contains('%')) {
        return External;
    }
    if let Some(origin) = origin.as_ref().filter(|origin| {
        origin.scheme() == "https"
            && origin.username().is_empty()
            && origin.password().is_none()
            && origin.port().is_none()
    }) {
        // Social sign-in redirects must keep GitHub's OAuth state in this store.
        if (page_sign_in_provider(&destination)
            && (github_sign_in_url(origin) || page_sign_in_provider(origin)))
            || (page_sign_in_provider(origin) && github_sign_in_url(&destination))
        {
            return Allow;
        }
    }
    let store = match destination.host_str() {
        Some("github.com") => {
            // Authentication and account-wide pages must retain the selected login.
            if destination.path() == "/"
                || matches!(
                    first,
                    "login" | "logout" | "session" | "sessions" | "settings"
                )
            {
                if same_service {
                    context.store.clone()
                } else {
                    PageStore::Default
                }
            } else {
                let current_owner = origin
                    .as_ref()
                    .filter(|url| url.host_str() == Some("github.com"))
                    .and_then(|url| url.path_segments()?.next());
                let canonical_pr = segments.get(2) == Some(&"pull")
                    && segments
                        .get(3)
                        .is_some_and(|number| number.parse::<u64>().is_ok_and(|number| number > 0));
                let mapped = crate::prs::account_for(first, &context.github_accounts).is_some();
                if !mapped && !canonical_pr && current_owner != Some(first) {
                    return External;
                }
                PageStore::github(first, &context.github_accounts)
            }
        }
        Some("linear.app") => {
            if destination.path() == "/"
                || first == "login"
                || destination.path() == "/oauth/authorize"
            {
                if same_service {
                    context.store.clone()
                } else {
                    PageStore::Default
                }
            } else if let Some(workspace) = crate::issues::workspace_from_issue_url(url) {
                PageStore::linear(Some(&workspace))
            } else {
                let origin_workspace = origin
                    .as_ref()
                    .and_then(|url| crate::issues::workspace_from_issue_url(url.as_str()));
                let current_workspace = match &context.store {
                    PageStore::Linear(workspace) => Some(workspace.as_str()),
                    _ => origin_workspace.as_deref(),
                };
                // Only known workspace prefixes select stores; a valid-looking
                // slug alone cannot distinguish a workspace from a global route.
                if context.linear_workspaces.contains(first)
                    || (same_service && current_workspace == Some(first))
                {
                    PageStore::linear(Some(first))
                } else {
                    return External;
                }
            }
        }
        _ => return External,
    }
    .normalized();
    if store == context.store {
        Allow
    } else {
        Reopen(store)
    }
}

#[cfg(test)]
#[path = "page_navigation_tests.rs"]
mod tests;
