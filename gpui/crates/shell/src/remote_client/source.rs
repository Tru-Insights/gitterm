//! Captured Iced workspace-source identity. All methods run on a worker.
use super::{PreviewSourceHost, RemoteClient};
use gitterm_core::{
    remote::{config::RemoteAgentConfig, source::WorkspaceSource},
    status::GitStatusSnapshot,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub struct Spec {
    pub remote: RemoteAgentConfig,
    pub workspace_id: String,
    pub root: String,
}
impl Spec {
    pub(super) fn workspace_source(&self) -> WorkspaceSource<PreviewSourceHost> {
        WorkspaceSource::RemoteAgent {
            workspace_id: self.workspace_id.clone(),
            root: self.root.clone(),
            client: self.remote.clone().into(),
        }
    }
}
#[derive(Clone)]
pub(crate) struct RemoteSource {
    pub spec: Spec,
    pub client: Arc<dyn RemoteClient>,
}
impl RemoteSource {
    pub fn status(&self, owner: usize) -> GitStatusSnapshot {
        gitterm_core::remote::source::source_git_status_snapshot(
            owner,
            PathBuf::from(&self.spec.root),
            self.client.source_status(&self.spec),
        )
    }
    pub fn review(
        &self,
        owner: usize,
    ) -> Result<(GitStatusSnapshot, gitterm_gpui_review::model::Patches), String> {
        use gitterm_gpui_review::model::{Group, Patches};
        let status = self.status(owner);
        if let Some(error) = &status.error {
            return Err(error.clone());
        }
        if !status.is_git_repo {
            return Err(format!(
                "not a Git repository: {}",
                status.repo_path.display()
            ));
        }
        let mut patches = Patches::new();
        for (group, files) in [
            (Group::Staged, &status.staged),
            (Group::Unstaged, &status.unstaged),
            (Group::Untracked, &status.untracked),
        ] {
            for file in files {
                let lines =
                    self.client
                        .source_diff(&self.spec, file.path.clone(), file.is_staged)?;
                let patch = remote_patch(&file.path, &lines);
                patches.insert((group, file.path.clone()), patch.into());
            }
        }
        Ok((status, patches))
    }
}

// The agent returns Iced's line preview, not a unified file envelope. Supply
// only the envelope required by GPUI's existing diff renderer.
fn remote_patch(path: &str, lines: &[gitterm_core::git_diff::FileDiffLine]) -> String {
    use gitterm_core::{git_diff::DiffLineKind, review::patch::quote_git_path};
    let old = quote_git_path("a/", path);
    let new = quote_git_path("b/", path);
    let mut patch = format!("diff --git {old} {new}\n--- {old}\n+++ {new}\n");
    let untracked = lines.first().is_some_and(|line| {
        line.kind == DiffLineKind::Header && line.content.ends_with("(new file)")
    });
    if untracked {
        let count = lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Addition)
            .count();
        patch.push_str(&format!("@@ -0,0 +1,{count} @@ (new file)"));
        // Keep Iced's capped preview notice visible in the hunk heading while
        // declaring the number of lines actually transported to the renderer.
        for line in lines
            .iter()
            .skip(1)
            .filter(|line| line.kind == DiffLineKind::Header)
        {
            patch.push(' ');
            patch.push_str(&line.content);
        }
        patch.push('\n');
    }
    for line in lines {
        if untracked && line.kind == DiffLineKind::Header {
            continue;
        }
        patch.push_str(match line.kind {
            DiffLineKind::Addition => "+",
            DiffLineKind::Deletion => "-",
            DiffLineKind::Context => " ",
            DiffLineKind::Header => "",
        });
        patch.push_str(&line.content);
        patch.push('\n');
    }
    patch
}

#[cfg(test)]
pub(crate) mod tests;
