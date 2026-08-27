use std::path::{Path, PathBuf};

use mf_vcs::git::{Git, GitStatus};
use mf_vcs::p4::P4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeBackend {
    P4,
    Git,
    None,
}

#[derive(Clone, Debug)]
pub struct ChangeEntry {
    pub path: PathBuf,
    pub status: String,
    pub change: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChangeSetSnapshot {
    pub backend: ChangeBackend,
    pub entries: Vec<ChangeEntry>,
}

impl ChangeSetSnapshot {
    pub fn load(workspace: &Path) -> Self {
        let p4 = P4::new(workspace);
        if let Ok(info) = p4.info() {
            let client_root = PathBuf::from(info.client_root);
            if !path_is_within(workspace, &client_root) {
                return load_git(workspace);
            }
            let entries = p4
                .opened()
                .unwrap_or_default()
                .into_iter()
                .filter(|file| path_is_within(&file.local_path(), workspace))
                .map(|file| ChangeEntry {
                    path: file.local_path(),
                    status: file.action,
                    change: Some(file.change),
                })
                .collect();
            return Self {
                backend: ChangeBackend::P4,
                entries,
            };
        }

        load_git(workspace)
    }

    pub fn label(&self) -> &'static str {
        match self.backend {
            ChangeBackend::P4 => "P4 CHANGE SET",
            ChangeBackend::Git => "GIT CHANGE SET",
            ChangeBackend::None => "CHANGE SET",
        }
    }
}

fn load_git(workspace: &Path) -> ChangeSetSnapshot {
    if let Ok(git) = Git::open(workspace) {
        let entries = git
            .status()
            .unwrap_or_default()
            .into_iter()
            .map(|file| ChangeEntry {
                path: file.path,
                status: git_status_label(&file.status).to_string(),
                change: None,
            })
            .collect();
        return ChangeSetSnapshot {
            backend: ChangeBackend::Git,
            entries,
        };
    }
    ChangeSetSnapshot {
        backend: ChangeBackend::None,
        entries: Vec::new(),
    }
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
        let root = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase();
        path == root || path.starts_with(&(root + "\\"))
    } else {
        path == root || path.starts_with(root)
    }
}

fn git_status_label(status: &GitStatus) -> &'static str {
    match status {
        GitStatus::New => "add",
        GitStatus::Modified => "edit",
        GitStatus::Deleted => "delete",
        GitStatus::Renamed => "rename",
        GitStatus::Staged { .. } => "staged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_statuses_map_to_delivery_language() {
        assert_eq!(git_status_label(&GitStatus::New), "add");
        assert_eq!(git_status_label(&GitStatus::Modified), "edit");
        assert_eq!(git_status_label(&GitStatus::Deleted), "delete");
        assert_eq!(
            git_status_label(&GitStatus::Staged {
                kind: Box::new(GitStatus::Modified),
            }),
            "staged"
        );
    }

    #[test]
    fn workspace_filter_does_not_match_sibling_prefixes() {
        let root = Path::new("C:/work/game");
        assert!(path_is_within(Path::new("C:/work/game/src/main.rs"), root));
        assert!(!path_is_within(Path::new("C:/work/game-tools/tool.rs"), root));
    }
}
