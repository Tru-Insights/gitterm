//! Runtime token lookup for a saved `token_ref`: every desktop host reads a
//! remote's credential through this one path and never copies or stores it.

use std::path::PathBuf;

use super::client::RemoteAgentClientError;

/// Read the token a `token_ref` names: `env:VAR`, `file:~/path` or
/// `keychain:service/account` (macOS).
pub fn resolve_token_ref(token_ref: &str) -> Result<String, RemoteAgentClientError> {
    let token_ref = token_ref.trim();
    if token_ref.is_empty() {
        return Err(RemoteAgentClientError::new("token_ref must not be empty"));
    }

    if let Some(name) = token_ref.strip_prefix("env:") {
        let name = name.trim();
        if name.is_empty() {
            return Err(RemoteAgentClientError::new(
                "env token_ref is missing a variable name",
            ));
        }
        return std::env::var(name).map_err(|err| {
            RemoteAgentClientError::new(format!("could not read token from env:{name}: {err}"))
        });
    }

    if let Some(path) = token_ref.strip_prefix("file:") {
        let path = expand_home_path(path.trim());
        let token = std::fs::read_to_string(&path).map_err(|err| {
            RemoteAgentClientError::new(format!(
                "could not read token file {}: {err}",
                path.display()
            ))
        })?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(RemoteAgentClientError::new(format!(
                "token file {} is empty",
                path.display()
            )));
        }
        return Ok(token);
    }

    if let Some(keychain_ref) = token_ref.strip_prefix("keychain:") {
        return read_keychain_token(keychain_ref.trim());
    }

    Err(RemoteAgentClientError::new(format!(
        "unsupported token_ref {token_ref}; expected env:, file:, or keychain:"
    )))
}

/// A leading `~/` expanded to the user's home directory.
pub fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(target_os = "macos")]
fn read_keychain_token(keychain_ref: &str) -> Result<String, RemoteAgentClientError> {
    let (service, account) = keychain_ref.rsplit_once('/').ok_or_else(|| {
        RemoteAgentClientError::new("keychain token_ref must be keychain:service/account")
    })?;
    if service.is_empty() || account.is_empty() {
        return Err(RemoteAgentClientError::new(
            "keychain token_ref must include service and account",
        ));
    }

    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|err| RemoteAgentClientError::new(format!("could not run security: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr
        };
        return Err(RemoteAgentClientError::new(format!(
            "could not read keychain token for {service}/{account}: {detail}"
        )));
    }

    let token = String::from_utf8(output.stdout)
        .map_err(|err| RemoteAgentClientError::new(format!("keychain token is not UTF-8: {err}")))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(RemoteAgentClientError::new(format!(
            "keychain token for {service}/{account} is empty"
        )));
    }
    Ok(token)
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_token(_keychain_ref: &str) -> Result<String, RemoteAgentClientError> {
    Err(RemoteAgentClientError::new(
        "keychain token refs are only supported on macOS",
    ))
}
