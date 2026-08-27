use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_VERSION: u32 = 1;

/// A work item's product-facing lifecycle.
///
/// This is deliberately independent from agent task statuses and the legacy
/// board lanes. A run may contain many steps, while one work item moves through
/// this lifecycle as a whole.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkItemPhase {
    #[default]
    Draft,
    Running,
    NeedsInput,
    Review,
    ReadyToDeliver,
    Done,
    Failed,
}

impl WorkItemPhase {
    /// Transitional mapping from the four lanes used by the old workspace board.
    pub fn from_workspace_status(status: &str) -> Self {
        match status {
            "in-progress" => Self::Running,
            "in-review" => Self::Review,
            "completed" => Self::Done,
            _ => Self::Draft,
        }
    }

    /// Transitional mapping back to a legacy workspace-board lane.
    pub fn as_workspace_status(self) -> &'static str {
        match self {
            Self::Draft | Self::Failed => "todo",
            Self::Running | Self::NeedsInput => "in-progress",
            Self::Review | Self::ReadyToDeliver => "in-review",
            Self::Done => "completed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub workspace: PathBuf,
    pub vcs_ref: String,
    #[serde(default)]
    pub phase: WorkItemPhase,
    #[serde(default)]
    pub run_id: Option<i64>,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// Workspace metadata discovered from Git, Perforce, or the project root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkItemSeed {
    pub name: String,
    pub workspace: PathBuf,
    pub vcs_ref: String,
}

impl WorkItemSeed {
    pub fn new(
        name: impl Into<String>,
        workspace: impl Into<PathBuf>,
        vcs_ref: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            workspace: workspace.into(),
            vcs_ref: vcs_ref.into(),
        }
    }
}

/// Persistent collection of work items belonging to one project.
///
/// Mutations are intentionally in-memory. Call [`Self::save`] at the UI or
/// application transaction boundary so write failures can be surfaced there.
#[derive(Clone, Debug)]
pub struct WorkItemStore {
    project_root: PathBuf,
    items: Vec<WorkItem>,
    active_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkItemFile {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    items: Vec<WorkItem>,
    #[serde(default)]
    active_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyBoardFile {
    #[serde(default)]
    cards: Vec<LegacyWorkspaceCard>,
}

#[derive(Debug, Deserialize)]
struct LegacyWorkspaceCard {
    #[serde(default)]
    name: String,
    path: PathBuf,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    last_activity: u64,
}

fn store_version() -> u32 {
    STORE_VERSION
}

impl WorkItemStore {
    /// Loads the project store. A missing or malformed file produces an empty
    /// usable store; reconciliation can then rebuild items from live workspaces.
    pub fn load(project_root: impl Into<PathBuf>) -> Self {
        let project_root = absolute_normalized(project_root.into(), None);
        let stored = fs::read_to_string(store_path(&project_root))
            .ok()
            .and_then(|text| serde_json::from_str::<WorkItemFile>(&text).ok())
            .unwrap_or_else(|| load_legacy_board(&project_root));

        let mut store = Self {
            project_root,
            items: stored.items,
            active_id: stored.active_id,
        };
        store.repair_loaded_state();
        store
    }

    pub fn save(&self) -> io::Result<()> {
        let directory = self.project_root.join(".mf-agent");
        fs::create_dir_all(&directory)?;
        let file = WorkItemFile {
            version: STORE_VERSION,
            items: self.items.clone(),
            active_id: self.active_id.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        fs::write(directory.join("work-items.json"), bytes)
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn items(&self) -> &[WorkItem] {
        &self.items
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }

    pub fn active(&self) -> Option<&WorkItem> {
        let active_id = self.active_id.as_deref()?;
        self.items.iter().find(|item| item.id == active_id)
    }

    /// Reconciles discovered workspaces with persisted work items.
    ///
    /// Matching primarily uses normalized workspace paths, then the stable ID.
    /// User-owned state (phase, run, comment, attention, timestamps) is kept;
    /// discovery-owned metadata (title, path, VCS ref) is refreshed.
    pub fn reconcile_workspaces(&mut self, seeds: impl IntoIterator<Item = WorkItemSeed>) {
        let old_items = std::mem::take(&mut self.items);
        let old_active = self.active_id.take();
        let mut consumed = HashSet::new();
        let mut ids = HashSet::new();
        let mut reconciled = Vec::new();

        for seed in seeds {
            let workspace = absolute_normalized(seed.workspace.clone(), Some(&self.project_root));
            let workspace_key = normalized_path_key(&workspace, &self.project_root);
            if reconciled.iter().any(|item: &WorkItem| {
                normalized_path_key(&item.workspace, &self.project_root) == workspace_key
            }) {
                continue;
            }

            let candidate_id = stable_id(&self.project_root, &seed.name, &workspace);
            let existing_index = old_items
                .iter()
                .enumerate()
                .find(|(index, item)| {
                    !consumed.contains(index)
                        && normalized_path_key(&item.workspace, &self.project_root) == workspace_key
                })
                .map(|(index, _)| index)
                .or_else(|| {
                    old_items
                        .iter()
                        .enumerate()
                        .find(|(index, item)| !consumed.contains(index) && item.id == candidate_id)
                        .map(|(index, _)| index)
                });

            let mut item = if let Some(index) = existing_index {
                consumed.insert(index);
                let mut item = old_items[index].clone();
                let title = seed_title(&seed, &workspace);
                if item.title != title
                    || item.workspace != workspace
                    || item.vcs_ref != seed.vcs_ref
                {
                    item.title = title;
                    item.workspace = workspace.clone();
                    item.vcs_ref = seed.vcs_ref;
                    touch(&mut item);
                }
                item
            } else {
                let timestamp = now_millis();
                WorkItem {
                    id: candidate_id,
                    title: seed_title(&seed, &workspace),
                    workspace: workspace.clone(),
                    vcs_ref: seed.vcs_ref,
                    phase: WorkItemPhase::Draft,
                    run_id: None,
                    comment: String::new(),
                    unread: false,
                    created_at: timestamp,
                    updated_at: timestamp,
                }
            };

            if ids.contains(&item.id) {
                item.id = path_fallback_id(&workspace, &self.project_root);
            }
            ids.insert(item.id.clone());
            reconciled.push(item);
        }

        self.items = reconciled;
        self.active_id = old_active.filter(|id| self.items.iter().any(|item| &item.id == id));
        if self.active_id.is_none() {
            self.active_id = self
                .items
                .iter()
                .find(|item| item.id == "main")
                .or_else(|| self.items.first())
                .map(|item| item.id.clone());
        }
    }

    pub fn find_by_workspace(&self, workspace: impl AsRef<Path>) -> Option<&WorkItem> {
        let key = normalized_path_key(workspace.as_ref(), &self.project_root);
        self.items
            .iter()
            .find(|item| normalized_path_key(&item.workspace, &self.project_root) == key)
    }

    pub fn activate_workspace(&mut self, workspace: impl AsRef<Path>) -> bool {
        let id = self
            .find_by_workspace(workspace)
            .map(|item| item.id.clone());
        let Some(id) = id else {
            return false;
        };
        self.active_id = Some(id.clone());
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            if item.unread {
                item.unread = false;
                touch(item);
            }
        }
        true
    }

    pub fn bind_run(&mut self, run_id: i64) -> bool {
        let Some(item) = self.active_mut() else {
            return false;
        };
        if item.run_id != Some(run_id) {
            item.run_id = Some(run_id);
            touch(item);
        }
        true
    }

    pub fn set_phase_for_active(&mut self, phase: WorkItemPhase) -> bool {
        let Some(item) = self.active_mut() else {
            return false;
        };
        if item.phase != phase {
            item.phase = phase;
            touch(item);
        }
        true
    }

    pub fn set_phase(&mut self, workspace: impl AsRef<Path>, phase: WorkItemPhase) -> bool {
        let key = normalized_path_key(workspace.as_ref(), &self.project_root);
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| normalized_path_key(&item.workspace, &self.project_root) == key)
        else {
            return false;
        };
        if item.phase != phase {
            item.phase = phase;
            touch(item);
        }
        true
    }

    pub fn update_comment(
        &mut self,
        workspace: impl AsRef<Path>,
        comment: impl Into<String>,
    ) -> bool {
        let key = normalized_path_key(workspace.as_ref(), &self.project_root);
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| normalized_path_key(&item.workspace, &self.project_root) == key)
        else {
            return false;
        };
        let comment = comment.into();
        if item.comment != comment {
            item.comment = comment;
            touch(item);
        }
        true
    }

    pub fn update_comment_for_active(&mut self, comment: impl Into<String>) -> bool {
        let Some(item) = self.active_mut() else {
            return false;
        };
        let comment = comment.into();
        if item.comment != comment {
            item.comment = comment;
            touch(item);
        }
        true
    }

    pub fn mark_unread(&mut self, workspace: impl AsRef<Path>) -> bool {
        self.set_unread(workspace.as_ref(), true)
    }

    pub fn clear_unread(&mut self, workspace: impl AsRef<Path>) -> bool {
        self.set_unread(workspace.as_ref(), false)
    }

    pub fn set_unread_for_active(&mut self, unread: bool) -> bool {
        let Some(item) = self.active_mut() else {
            return false;
        };
        if item.unread != unread {
            item.unread = unread;
            touch(item);
        }
        true
    }

    pub fn set_unread(&mut self, workspace: impl AsRef<Path>, unread: bool) -> bool {
        let workspace = workspace.as_ref();
        let key = normalized_path_key(workspace, &self.project_root);
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| normalized_path_key(&item.workspace, &self.project_root) == key)
        else {
            return false;
        };
        if item.unread != unread {
            item.unread = unread;
            touch(item);
        }
        true
    }

    fn active_mut(&mut self) -> Option<&mut WorkItem> {
        let active_id = self.active_id.as_deref()?;
        self.items.iter_mut().find(|item| item.id == active_id)
    }

    fn repair_loaded_state(&mut self) {
        let mut seen_ids = HashSet::new();
        let mut seen_paths = HashSet::new();
        self.items.retain(|item| {
            !item.id.is_empty()
                && seen_ids.insert(item.id.clone())
                && seen_paths.insert(normalized_path_key(&item.workspace, &self.project_root))
        });
        if !self
            .active_id
            .as_ref()
            .is_some_and(|id| self.items.iter().any(|item| &item.id == id))
        {
            self.active_id = None;
        }
    }
}

fn store_path(project_root: &Path) -> PathBuf {
    project_root.join(".mf-agent").join("work-items.json")
}

fn load_legacy_board(project_root: &Path) -> WorkItemFile {
    let legacy = fs::read_to_string(project_root.join(".mf-agent/workspaces.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<LegacyBoardFile>(&text).ok())
        .unwrap_or_default();
    let items = legacy
        .cards
        .into_iter()
        .map(|card| {
            let workspace = absolute_normalized(card.path, Some(project_root));
            let timestamp = card.last_activity.saturating_mul(1000);
            WorkItem {
                id: stable_id(project_root, &card.name, &workspace),
                title: if card.name.is_empty() {
                    project_root
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("主工作项")
                        .to_string()
                } else {
                    card.name
                },
                workspace,
                vcs_ref: card.branch,
                phase: WorkItemPhase::from_workspace_status(&card.status),
                run_id: None,
                comment: card.comment,
                unread: card.unread,
                created_at: timestamp,
                updated_at: timestamp,
            }
        })
        .collect();
    WorkItemFile {
        version: STORE_VERSION,
        items,
        active_id: None,
    }
}

fn seed_title(seed: &WorkItemSeed, workspace: &Path) -> String {
    let name = seed.name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("工作项")
        .to_owned()
}

fn stable_id(project_root: &Path, name: &str, workspace: &Path) -> String {
    if same_path(project_root, workspace, project_root) {
        return "main".to_owned();
    }
    let name = name.trim();
    if !name.is_empty() && name != "main" {
        return name.to_owned();
    }
    path_fallback_id(workspace, project_root)
}

fn path_fallback_id(workspace: &Path, project_root: &Path) -> String {
    let key = normalized_path_key(workspace, project_root);
    let hash = key.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("workspace-{hash:016x}")
}

fn same_path(left: &Path, right: &Path, project_root: &Path) -> bool {
    normalized_path_key(left, project_root) == normalized_path_key(right, project_root)
}

fn normalized_path_key(path: &Path, project_root: &Path) -> String {
    let normalized = absolute_normalized(path.to_path_buf(), Some(project_root));
    let key = normalized.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn absolute_normalized(path: PathBuf, base: Option<&Path>) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else if let Some(base) = base {
        base.join(path)
    } else {
        std::env::current_dir().map_or(path.clone(), |cwd| cwd.join(path))
    };
    lexical_normalize(&absolute)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn touch(item: &mut WorkItem) {
    item.updated_at = now_millis().max(item.updated_at.saturating_add(1));
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestProject(PathBuf);

    impl TestProject {
        fn new(label: &str) -> Self {
            let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "monkeyfence-work-items-{label}-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn seeds(project: &Path) -> Vec<WorkItemSeed> {
        vec![
            WorkItemSeed::new("MonkeyFence", project, "main"),
            WorkItemSeed::new(
                "navigation",
                project.join(".worktrees").join("navigation"),
                "codex/navigation",
            ),
        ]
    }

    #[test]
    fn load_save_roundtrip_preserves_items_and_active_selection() {
        let project = TestProject::new("roundtrip");
        let mut store = WorkItemStore::load(&project.0);
        store.reconcile_workspaces(seeds(&project.0));
        assert!(store.activate_workspace(project.0.join(".worktrees/navigation")));
        assert!(store.bind_run(42));
        assert!(store.set_phase_for_active(WorkItemPhase::Review));
        assert!(store.update_comment_for_active("ready to inspect"));
        assert!(store.set_unread_for_active(true));
        store.save().unwrap();

        let loaded = WorkItemStore::load(&project.0);
        assert_eq!(loaded.active_id(), Some("navigation"));
        assert_eq!(loaded.items(), store.items());
        assert_eq!(loaded.active().unwrap().run_id, Some(42));
    }

    #[test]
    fn reconcile_preserves_owned_state_and_refreshes_discovery_metadata() {
        let project = TestProject::new("reconcile");
        let workspace = project.0.join(".worktrees/navigation");
        let mut store = WorkItemStore::load(&project.0);
        store.reconcile_workspaces(seeds(&project.0));
        assert!(store.activate_workspace(&workspace));
        assert!(store.bind_run(7));
        assert!(store.set_phase_for_active(WorkItemPhase::NeedsInput));
        assert!(store.update_comment(&workspace, "waiting for API choice"));
        assert!(store.mark_unread(&workspace));
        let original_created_at = store.active().unwrap().created_at;

        store.reconcile_workspaces([
            WorkItemSeed::new("MonkeyFence", &project.0, "trunk"),
            WorkItemSeed::new("navigation", &workspace, "codex/navigation-v2"),
        ]);

        let item = store.find_by_workspace(&workspace).unwrap();
        assert_eq!(item.id, "navigation");
        assert_eq!(item.vcs_ref, "codex/navigation-v2");
        assert_eq!(item.phase, WorkItemPhase::NeedsInput);
        assert_eq!(item.run_id, Some(7));
        assert_eq!(item.comment, "waiting for API choice");
        assert!(item.unread);
        assert_eq!(item.created_at, original_created_at);
        assert_eq!(store.active_id(), Some("navigation"));
    }

    #[test]
    fn activation_uses_normalized_workspace_path_and_clears_attention() {
        let project = TestProject::new("activation");
        let mut store = WorkItemStore::load(&project.0);
        store.reconcile_workspaces(seeds(&project.0));
        let workspace = project.0.join(".worktrees/navigation");
        assert!(store.mark_unread(&workspace));

        let equivalent = workspace.join("child").join("..");
        assert!(store.activate_workspace(equivalent));
        assert_eq!(store.active_id(), Some("navigation"));
        assert!(!store.active().unwrap().unread);
        assert!(!store.activate_workspace(project.0.join("missing")));
    }

    #[test]
    fn phase_and_run_binding_only_mutate_the_active_item() {
        let project = TestProject::new("binding");
        let mut store = WorkItemStore::load(&project.0);
        store.reconcile_workspaces(seeds(&project.0));
        assert_eq!(store.active_id(), Some("main"));
        assert!(store.bind_run(11));
        assert!(store.set_phase_for_active(WorkItemPhase::Running));

        let workspace = project.0.join(".worktrees/navigation");
        assert!(store.activate_workspace(&workspace));
        assert!(store.bind_run(12));
        assert!(store.set_phase_for_active(WorkItemPhase::ReadyToDeliver));

        let main = store.find_by_workspace(&project.0).unwrap();
        assert_eq!(main.run_id, Some(11));
        assert_eq!(main.phase, WorkItemPhase::Running);
        let navigation = store.find_by_workspace(&workspace).unwrap();
        assert_eq!(navigation.run_id, Some(12));
        assert_eq!(navigation.phase, WorkItemPhase::ReadyToDeliver);
    }

    #[test]
    fn identifiers_are_stable_and_corrupt_storage_is_tolerated() {
        let project = TestProject::new("stable-id");
        fs::create_dir_all(project.0.join(".mf-agent")).unwrap();
        fs::write(project.0.join(".mf-agent/work-items.json"), "not json").unwrap();
        let unnamed = project.0.join(".worktrees/unnamed");

        let mut first = WorkItemStore::load(&project.0);
        assert!(first.items().is_empty());
        first.reconcile_workspaces([
            WorkItemSeed::new("root title", &project.0, "main"),
            WorkItemSeed::new("", &unnamed, "detached"),
        ]);
        let ids: Vec<_> = first.items().iter().map(|item| item.id.clone()).collect();
        first.save().unwrap();

        let mut second = WorkItemStore::load(&project.0);
        second.reconcile_workspaces([
            WorkItemSeed::new("renamed root", &project.0, "main"),
            WorkItemSeed::new("", &unnamed, "detached"),
        ]);
        assert_eq!(
            second
                .items()
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(ids[0], "main");
        assert!(ids[1].starts_with("workspace-"));
    }

    #[test]
    fn legacy_workspace_cards_are_migrated() {
        let project = TestProject::new("legacy");
        fs::create_dir_all(project.0.join(".mf-agent")).unwrap();
        let legacy = serde_json::json!({
            "cards": [{
                "name": "legacy-fix",
                "path": project.0.join(".worktrees/legacy-fix"),
                "branch": "legacy-fix",
                "status": "in-review",
                "comment": "review me",
                "unread": true,
                "last_activity": 42
            }]
        });
        fs::write(
            project.0.join(".mf-agent/workspaces.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let store = WorkItemStore::load(&project.0);
        let item = &store.items()[0];
        assert_eq!(item.id, "legacy-fix");
        assert_eq!(item.phase, WorkItemPhase::Review);
        assert_eq!(item.comment, "review me");
        assert!(item.unread);
        assert_eq!(item.updated_at, 42_000);
    }

    #[test]
    fn legacy_workspace_status_mapping_is_explicit() {
        assert_eq!(
            WorkItemPhase::from_workspace_status("in-progress"),
            WorkItemPhase::Running
        );
        assert_eq!(
            WorkItemPhase::NeedsInput.as_workspace_status(),
            "in-progress"
        );
        assert_eq!(
            WorkItemPhase::ReadyToDeliver.as_workspace_status(),
            "in-review"
        );
        assert_eq!(WorkItemPhase::Failed.as_workspace_status(), "todo");
    }
}
