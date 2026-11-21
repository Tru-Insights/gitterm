//! Login-store identity for a remote GitHub or Linear Page.
//!
//! The identifier scheme matches the Iced host's `webview::PageDataStore`, so
//! each host keys its WebKit stores the same way. Each app keeps the stores in
//! its own WebKit data (bundle-scoped), so hosts never share a login.
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageStore {
    Default,
    GitHub(String),
    Linear(String),
}

impl PageStore {
    /// The store for a PR owned by `owner`: its mapped gh account, else default.
    pub fn github(owner: &str, accounts: &BTreeMap<String, String>) -> Self {
        crate::prs::account_for(owner, accounts)
            // GitHub logins are case-insensitive.
            .map(|login| Self::GitHub(login.to_ascii_lowercase()))
            .unwrap_or(Self::Default)
    }

    /// The store for an issue in Linear `workspace`, else default.
    pub fn linear(workspace: Option<&str>) -> Self {
        workspace
            .filter(|slug| !slug.is_empty())
            .map(|slug| Self::Linear(slug.to_string()))
            .unwrap_or(Self::Default)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Default => "default login".to_string(),
            Self::GitHub(login) => format!("as {login}"),
            Self::Linear(workspace) => format!("as {workspace}"),
        }
    }

    /// A stable, namespaced 16-byte store identifier; `None` for the default store.
    pub fn identifier(&self) -> Option<[u8; 16]> {
        let (namespace, key) = match self {
            Self::Default => return None,
            Self::GitHub(login) => ("github", login.to_ascii_lowercase()),
            Self::Linear(workspace) => ("linear", workspace.clone()),
        };
        let mut input = b"gitterm-v6-webkit-store-v1\0".to_vec();
        input.extend_from_slice(namespace.as_bytes());
        input.push(0);
        input.extend_from_slice(key.as_bytes());
        let digest = ring::digest::digest(&ring::digest::SHA256, &input);
        let mut identifier = [0; 16];
        identifier.copy_from_slice(&digest.as_ref()[..16]);
        Some(identifier)
    }
}

/// Only HTTP(S) URLs may open in a Page or the external browser.
pub fn browser_url(url: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("refused URL scheme {}", parsed.scheme()));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_match_the_iced_scheme_and_stay_distinct() {
        let alpha = PageStore::GitHub("sample-login-a".to_string());
        // Same vector as the Iced host's `page_store_identifiers_are_stable_and_distinct`.
        assert_eq!(
            alpha.identifier(),
            Some([
                0xa9, 0x0d, 0x78, 0xb9, 0x36, 0xe7, 0x7e, 0x9f, 0x1e, 0xe4, 0x3b, 0x89, 0xd3, 0xf8,
                0xa9, 0x35,
            ])
        );
        assert_ne!(
            alpha.identifier(),
            PageStore::GitHub("sample-login-b".to_string()).identifier()
        );
        assert_ne!(
            alpha.identifier(),
            PageStore::Linear("sample-login-a".to_string()).identifier()
        );
        assert_eq!(PageStore::Default.identifier(), None);
    }

    #[test]
    fn stores_follow_accounts_and_workspaces() {
        let accounts = BTreeMap::from([("Sample-Owner".to_string(), "Sample-Login".to_string())]);
        assert_eq!(
            PageStore::github("sample-owner", &accounts),
            PageStore::GitHub("sample-login".to_string())
        );
        assert_eq!(
            PageStore::github("another-owner", &accounts),
            PageStore::Default
        );
        assert_eq!(
            PageStore::linear(Some("sample-workspace")),
            PageStore::Linear("sample-workspace".to_string())
        );
        assert_eq!(PageStore::linear(Some("")), PageStore::Default);
        assert_eq!(PageStore::linear(None), PageStore::Default);
    }

    #[test]
    fn browser_urls_allow_only_http_and_https() {
        assert!(browser_url("https://example.com/a").is_ok());
        assert!(browser_url("http://example.com/a").is_ok());
        for url in ["file:///etc/hosts", "javascript:alert(1)", "not a url"] {
            assert!(browser_url(url).is_err(), "{url}");
        }
    }
}
