//! Shared desktop connect-form persistence. Call on a blocking executor; the
//! desktop host owns the V6 root lease. Draft credentials never implement Debug.
use std::{fs, io::Write as _, path::Path};

use super::{
    config::{RemoteAgentAuthConfig, RemoteAgentConfig, RemoteAgentsFile, REMOTE_AGENTS_FILE},
    state::Identity,
};

pub const REQUIRED: &str = "Name, endpoint, and token are all required";
pub const INVALID_ENDPOINT: &str =
    "Endpoint must be an http:// or https:// URL without credentials, query, or fragment";
pub const IDENTITY_CHANGED: &str =
    "Remote agent identity changed; close this form and connect the host again";
pub const MALFORMED: &str =
    "Could not read remote-agents.json: invalid JSON; existing file was preserved";

pub struct ConnectDraft<'a> {
    pub name: &'a str,
    pub endpoint: &'a str,
    pub token: &'a str,
    /// The exact identity saved by this form, retained across retries.
    pub saved: Option<&'a Identity>,
}

pub fn remote_agent_id_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "remote".into()
    } else {
        slug.into()
    }
}

/// Update only fields edited by the form; retain hand-maintained commands.
pub fn upsert_remote_agent_config(
    definitions: &mut Vec<RemoteAgentConfig>,
    agent: RemoteAgentConfig,
) {
    if let Some(existing) = definitions
        .iter_mut()
        .find(|existing| existing.id == agent.id)
    {
        existing.name = agent.name;
        existing.endpoint = agent.endpoint;
        existing.auth = agent.auth;
    } else {
        definitions.push(agent);
    }
}

/// Errors deliberately exclude parser input, endpoints and credential values.
pub fn load_remote_agents(root: &Path) -> Result<RemoteAgentsFile, String> {
    match fs::read(root.join(REMOTE_AGENTS_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| MALFORMED.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RemoteAgentsFile::default())
        }
        Err(error) => Err(format!(
            "Could not read remote-agents.json: {}",
            error.kind()
        )),
    }
}

/// Iced's field order, normalization, collision suffixes, token references and
/// pretty JSON, shared by both desktop forms. Refuse bad data before any write.
pub fn save_remote_connect(
    root: &Path,
    known: &[RemoteAgentConfig],
    draft: ConnectDraft<'_>,
) -> Result<RemoteAgentConfig, String> {
    let name = draft.name.trim();
    let raw = draft.endpoint.trim();
    let token = draft.token.trim();
    if name.is_empty() || raw.is_empty() || token.is_empty() {
        return Err(REQUIRED.into());
    }
    let endpoint = if raw.contains("://") {
        raw.into()
    } else {
        format!("http://{raw}")
    };
    Identity::new("validation".into(), endpoint.clone())
        .map_err(|_| INVALID_ENDPOINT.to_string())?;
    let mut file = load_remote_agents(root)?;
    let id = match draft.saved {
        Some(saved) => {
            let id = saved.remote_id();
            // A saved id is also a filename; never let a caller traverse tokens/.
            if remote_agent_id_slug(id) != id
                || saved.endpoint() != endpoint
                || file
                    .remote_agents
                    .iter()
                    .chain(known)
                    .any(|agent| agent.id == id && agent.endpoint != saved.endpoint())
                || !file
                    .remote_agents
                    .iter()
                    .chain(known)
                    .any(|agent| agent.id == id)
            {
                return Err(IDENTITY_CHANGED.into());
            }
            id.to_string()
        }
        None => {
            let base = remote_agent_id_slug(name);
            let mut id = base.clone();
            let mut suffix = 2;
            while file
                .remote_agents
                .iter()
                .chain(known)
                .any(|agent| agent.id == id)
            {
                id = format!("{base}-{suffix}");
                suffix += 1;
            }
            id
        }
    };
    // Only explicit refs enter config; pasted credentials stay in the 0600 file.
    let token_ref = if token.starts_with("env:")
        || token.starts_with("file:")
        || token.starts_with("keychain:")
    {
        token.to_string()
    } else {
        write_remote_agent_token(root, &id, token)
            .map_err(|error| format!("Could not store token: {}", error.kind()))?
    };
    let agent = RemoteAgentConfig {
        id,
        name: name.into(),
        endpoint,
        auth: RemoteAgentAuthConfig::Token { token_ref },
        shell_command: None,
        session_commands: Default::default(),
    };
    upsert_remote_agent_config(&mut file.remote_agents, agent.clone());
    save_remote_agents(root, &file)?;
    Ok(agent)
}

/// Also used by Iced's ordinary catalog/shutdown save, so a later save cannot
/// undo the connect form's refusal to overwrite a malformed document.
pub fn save_remote_agents(root: &Path, file: &RemoteAgentsFile) -> Result<(), String> {
    load_remote_agents(root)?;
    fs::create_dir_all(root)
        .map_err(|e| format!("Could not save remote-agents.json: {}", e.kind()))?;
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|_| "Could not serialize remote-agents.json".to_string())?;
    crate::atomic_file::write_atomically(&root.join(REMOTE_AGENTS_FILE), &bytes)
        .map_err(|error| format!("Could not save remote-agents.json: {error}"))
}

fn write_remote_agent_token(root: &Path, id: &str, token: &str) -> std::io::Result<String> {
    let dir = root.join("tokens");
    fs::create_dir_all(&dir)?;
    if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(std::io::Error::other("tokens directory is a symlink"));
    }
    let path = dir.join(format!("{id}.token"));
    let temporary = dir.join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Keep Iced's exact mode even when umask removes owner bits.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        writeln!(file, "{token}")?;
        file.sync_all()?;
        crate::tasks::store::replace_file_atomically(&temporary, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(format!("file:{}", path.display()))
}

#[cfg(test)]
mod tests;
