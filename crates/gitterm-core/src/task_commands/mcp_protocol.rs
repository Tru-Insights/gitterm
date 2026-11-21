//! What a harness process sees of GitTerm's two loopback MCP servers (task
//! and browser): the environment variable names, the per-tab `caller`
//! query, and the zsh wrapper that hands both servers to a manual Codex
//! launch. Neither server owns these, so neither depends on the other.

pub const TASK_MCP_TOKEN_ENV: &str = "GITTERM_V6_TASK_MCP_TOKEN";
pub const TASK_MCP_URL_ENV: &str = "GITTERM_V6_TASK_MCP_URL";
pub const BROWSER_MCP_TOKEN_ENV: &str = "GITTERM_V6_BROWSER_MCP_TOKEN";
pub const BROWSER_MCP_URL_ENV: &str = "GITTERM_V6_BROWSER_MCP_URL";
/// Query parameter on a tab's task MCP URL naming the calling tab's durable
/// `session_uid`. The bearer token authenticates; this only attributes.
pub const TASK_MCP_CALLER_QUERY: &str = "caller";

/// The task MCP URL handed to one tab: the shared endpoint plus
/// `?caller=<session_uid>`. Without a caller the bare endpoint is returned.
pub fn caller_endpoint(endpoint: &str, caller: Option<&str>) -> String {
    match caller.filter(|caller| !caller.is_empty()) {
        Some(caller) => {
            let encoded: String = url::form_urlencoded::byte_serialize(caller.as_bytes()).collect();
            format!("{endpoint}?{TASK_MCP_CALLER_QUERY}={encoded}")
        }
        None => endpoint.to_string(),
    }
}

/// The caller named by a request URI's `caller` query, if any. The browser
/// MCP reads the same query (TRU-176).
pub fn caller_from_query(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == TASK_MCP_CALLER_QUERY)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// One zsh wrapper composes both GitTerm-owned MCP servers. It is safe when
/// either service is unavailable and never writes global Codex configuration.
pub fn codex_zsh_integration() -> String {
    format!(
        r#"if [[ ( -n "${{{browser_url}:-}}" && -n "${{{browser_token}:-}}" ) || ( -n "${{{task_url}:-}}" && -n "${{{task_token}:-}}" ) ]]; then
    codex() {{
        local arg
        for arg in "$@"; do
            if [[ "$arg" == mcp_servers.gitterm_browser.url=* || "$arg" == mcp_servers.gitterm_tasks.url=* ]]; then
                command codex "$@"
                return
            fi
        done
        local -a gitterm_mcp_args
        if [[ -n "${{{browser_url}:-}}" && -n "${{{browser_token}:-}}" ]]; then
            gitterm_mcp_args+=(
                --config "mcp_servers.gitterm_browser.url=${{{browser_url}}}"
                --config "mcp_servers.gitterm_browser.bearer_token_env_var={browser_token}"
                --config "mcp_servers.gitterm_browser.default_tools_approval_mode=writes"
                --config "mcp_servers.gitterm_browser.tool_timeout_sec=60"
            )
        fi
        if [[ -n "${{{task_url}:-}}" && -n "${{{task_token}:-}}" ]]; then
            gitterm_mcp_args+=(
                --config "mcp_servers.gitterm_tasks.url=${{{task_url}}}"
                --config "mcp_servers.gitterm_tasks.bearer_token_env_var={task_token}"
                --config "mcp_servers.gitterm_tasks.default_tools_approval_mode=approve"
                --config "mcp_servers.gitterm_tasks.tool_timeout_sec=300"
            )
        fi
        command codex "${{gitterm_mcp_args[@]}}" "$@"
    }}
fi"#,
        browser_url = BROWSER_MCP_URL_ENV,
        browser_token = BROWSER_MCP_TOKEN_ENV,
        task_url = TASK_MCP_URL_ENV,
        task_token = TASK_MCP_TOKEN_ENV,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_round_trips_through_the_endpoint_query() {
        let caller = "tab uid/&?=";
        let endpoint = caller_endpoint("http://127.0.0.1:1/mcp", Some(caller));
        let query = endpoint.split_once('?').map(|(_, query)| query);
        assert_eq!(caller_from_query(query).as_deref(), Some(caller));
        assert_eq!(caller_endpoint("http://e/mcp", Some("")), "http://e/mcp");
        assert_eq!(caller_endpoint("http://e/mcp", None), "http://e/mcp");
        assert_eq!(caller_from_query(Some("caller=")), None);
        assert_eq!(caller_from_query(Some("other=x")), None);
        assert_eq!(caller_from_query(None), None);
    }

    #[test]
    fn codex_integration_composes_browser_and_task_servers() {
        let integration = codex_zsh_integration();
        assert!(integration.contains("mcp_servers.gitterm_browser.url="));
        assert!(integration.contains("mcp_servers.gitterm_tasks.url="));
        assert!(integration.contains(BROWSER_MCP_URL_ENV));
        assert!(integration.contains(TASK_MCP_TOKEN_ENV));
        assert!(integration.contains("gitterm_mcp_args"));
        assert!(!integration.contains("Bearer "));
    }
}
