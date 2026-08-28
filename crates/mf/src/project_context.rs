//! 项目上下文 activation seam:唯一可信的「当前项目 / 当前任务」状态机。
//!
//! 纯 in-process 状态,不持有 GPUI Entity:
//! - ProjectId:规范化绝对路径的 newtype,全程序共用同一身份。
//! - ProjectContextState:原子激活语义(项目 + 任务一起切换),
//!   记录每项目最近选中的 Task 与全局激活顺序。
//! UI 只通过 `activate/remove_project/restore/snapshot` 表达意图,
//! 具体联动(FileTree/VCS/标签/终端/侧栏)由 Workspace 在拿到
//! ActivationOutcome 后统一执行。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------- ProjectId ----------

/// 规范化项目身份:打开项目时集中规范化一次。
#[derive(Debug, Clone)]
pub struct ProjectId(Arc<PathBuf>);

impl ProjectId {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn root(&self) -> PathBuf {
        self.0.as_ref().clone()
    }

    /// UI 展示名:目录名(不含 Windows `\\?\` 前缀)。
    pub fn display_name(&self) -> String {
        self.as_path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.as_path().display().to_string())
    }
}

impl PartialEq for ProjectId {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for ProjectId {}
impl std::hash::Hash for ProjectId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// 去掉 Windows canonicalize 产生的 `\\?\` 扩展长度前缀(仅普通盘符路径)。
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // 只处理 `\\?\C:\...` 形式;其他(命名管道等)原样保留
        if rest
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// 集中规范化:优先 canonicalize;失败(路径暂不存在)回退绝对路径并给出警告。
/// 相等性依赖 canonicalize 的真实大小写与 separator 归一,不做简单 lowercase。
pub fn normalize_project_path(path: &Path) -> (ProjectId, Option<String>) {
    match std::fs::canonicalize(path) {
        Ok(canonical) => (ProjectId(Arc::new(strip_verbatim(&canonical))), None),
        Err(_) => {
            let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            let warning = format!("路径无法规范化,使用绝对路径: {}", absolute.display());
            (
                ProjectId(Arc::new(strip_verbatim(&absolute))),
                Some(warning),
            )
        }
    }
}

/// 在可能嵌套的已打开项目中选择最深匹配项；父项目不能抢占子项目文件。
pub fn deepest_owning_project(projects: &[ProjectId], path: &Path) -> Option<ProjectId> {
    projects
        .iter()
        .filter(|project| path.starts_with(project.as_path()))
        .max_by_key(|project| project.as_path().components().count())
        .cloned()
}

// ---------- 激活语义 ----------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActiveProjectContext {
    pub project: Option<ProjectId>,
    pub task_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum ActivationTarget {
    Project(ProjectId),
    Task {
        project: ProjectId,
        task_id: i64,
    },
    Tab {
        project: ProjectId,
    },
    AgentRun {
        project: ProjectId,
        task_id: Option<i64>,
        session_id: i64,
    },
    Restore {
        project: ProjectId,
        task_id: Option<i64>,
    },
}

#[derive(Debug, Clone)]
pub struct ActivationOutcome {
    pub previous: ActiveProjectContext,
    pub current: ActiveProjectContext,
    pub project_changed: bool,
    pub task_changed: bool,
    pub mark_task_read: Option<(ProjectId, i64)>,
    pub mark_session_read: Option<(ProjectId, i64)>,
}

/// 纯状态机:known_projects 保持打开顺序,activation_order 记录激活新旧。
pub struct ProjectContextState {
    known_projects: Vec<ProjectId>,
    activation_order: Vec<ProjectId>,
    selected_task_by_project: HashMap<ProjectId, i64>,
    current: ActiveProjectContext,
}

impl Default for ProjectContextState {
    fn default() -> Self {
        Self {
            known_projects: Vec::new(),
            activation_order: Vec::new(),
            selected_task_by_project: HashMap::new(),
            current: ActiveProjectContext::default(),
        }
    }
}

impl ProjectContextState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开(注册)项目;已存在则不重复。注册后尚未激活。
    pub fn open_project(&mut self, id: ProjectId) {
        if !self.known_projects.contains(&id) {
            self.known_projects.push(id);
        }
    }

    pub fn snapshot(&self) -> ActiveProjectContext {
        self.current.clone()
    }

    pub fn known_projects(&self) -> &[ProjectId] {
        &self.known_projects
    }

    /// 项目最近选中的 Task(未选择过则 None)。
    pub fn last_task_of(&self, project: &ProjectId) -> Option<i64> {
        self.selected_task_by_project.get(project).copied()
    }

    pub fn activate(&mut self, target: ActivationTarget) -> ActivationOutcome {
        let previous = self.current.clone();
        let mark_session_read = match &target {
            ActivationTarget::AgentRun {
                project,
                session_id,
                ..
            } => Some((project.clone(), *session_id)),
            _ => None,
        };
        let (project, task_id, mark_task_read) = match target {
            ActivationTarget::Project(p) => {
                let task = if previous.project.as_ref() == Some(&p) {
                    previous.task_id
                } else {
                    self.selected_task_by_project.get(&p).copied()
                };
                (Some(p), task, None)
            }
            ActivationTarget::Tab { project: p } => {
                let task = if previous.project.as_ref() == Some(&p) {
                    previous.task_id
                } else {
                    self.selected_task_by_project.get(&p).copied()
                };
                (Some(p), task, None)
            }
            ActivationTarget::Task {
                project: p,
                task_id,
            } => {
                self.selected_task_by_project.insert(p.clone(), task_id);
                (Some(p.clone()), Some(task_id), Some((p, task_id)))
            }
            ActivationTarget::AgentRun {
                project: p,
                task_id,
                ..
            } => {
                let task = task_id.or_else(|| self.selected_task_by_project.get(&p).copied());
                if let Some(t) = task {
                    self.selected_task_by_project.insert(p.clone(), t);
                }
                (Some(p), task, None)
            }
            ActivationTarget::Restore {
                project: p,
                task_id,
            } => {
                let task = task_id.or_else(|| self.selected_task_by_project.get(&p).copied());
                if let Some(t) = task {
                    self.selected_task_by_project.insert(p.clone(), t);
                }
                (Some(p), task, None)
            }
        };
        if let Some(p) = &project {
            self.note_activation(p);
        }
        self.current = ActiveProjectContext { project, task_id };
        ActivationOutcome {
            project_changed: previous.project != self.current.project,
            task_changed: previous.task_id != self.current.task_id,
            previous,
            current: self.current.clone(),
            mark_task_read,
            mark_session_read,
        }
    }

    /// 关闭项目:移除注册、激活顺序与已选 Task;若为当前项目,
    /// 切换到最近激活的剩余项目(恢复其最近 Task),无剩余则上下文归零。
    pub fn remove_project(&mut self, id: &ProjectId) -> ActivationOutcome {
        self.known_projects.retain(|p| p != id);
        self.activation_order.retain(|p| p != id);
        self.selected_task_by_project.remove(id);
        let previous = self.current.clone();
        if previous.project.as_ref() == Some(id) {
            let next = self
                .activation_order
                .last()
                .cloned()
                .or_else(|| self.known_projects.last().cloned());
            let current = match next {
                Some(p) => {
                    let task = self.selected_task_by_project.get(&p).copied();
                    ActiveProjectContext {
                        project: Some(p),
                        task_id: task,
                    }
                }
                None => ActiveProjectContext::default(),
            };
            self.current = current;
        }
        ActivationOutcome {
            project_changed: previous.project != self.current.project,
            task_changed: previous.task_id != self.current.task_id,
            previous,
            current: self.current.clone(),
            mark_task_read: None,
            mark_session_read: None,
        }
    }

    /// Task 被删除/归档后调用:清除残留选择(含当前上下文)。
    pub fn task_gone(&mut self, project: &ProjectId, task_id: i64) {
        if self
            .selected_task_by_project
            .get(project)
            .is_some_and(|t| *t == task_id)
        {
            self.selected_task_by_project.remove(project);
        }
        if self.current.project.as_ref() == Some(project) && self.current.task_id == Some(task_id) {
            self.current.task_id = None;
        }
    }

    fn note_activation(&mut self, project: &ProjectId) {
        self.activation_order.retain(|p| p != project);
        self.activation_order.push(project.clone());
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn pid(path: &str) -> ProjectId {
        normalize_project_path(Path::new(path)).0
    }

    fn tmp_id(tag: &str) -> ProjectId {
        let dir = std::env::temp_dir().join(format!("mf-pctx-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        normalize_project_path(&dir).0
    }

    #[test]
    fn switch_project_a_to_b() {
        let a = tmp_id("a");
        let b = tmp_id("b");
        let mut s = ProjectContextState::new();
        s.open_project(a.clone());
        s.open_project(b.clone());
        let out = s.activate(ActivationTarget::Project(a.clone()));
        assert_eq!(out.current.project.as_ref(), Some(&a));
        assert!(out.project_changed);
        let out = s.activate(ActivationTarget::Project(b.clone()));
        assert_eq!(out.current.project.as_ref(), Some(&b));
        assert!(out.project_changed);
    }

    #[test]
    fn task_activation_switches_project_and_task() {
        let a = tmp_id("ta");
        let b = tmp_id("tb");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: a.clone(),
            task_id: 1,
        });
        let out = s.activate(ActivationTarget::Task {
            project: b.clone(),
            task_id: 5,
        });
        assert_eq!(out.current.project.as_ref(), Some(&b));
        assert_eq!(out.current.task_id, Some(5));
        assert!(out.project_changed && out.task_changed);
        assert_eq!(out.mark_task_read, Some((b.clone(), 5)));
    }

    #[test]
    fn tab_activation_restores_last_task_of_project() {
        let a = tmp_id("taba");
        let b = tmp_id("tabb");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: a.clone(),
            task_id: 7,
        });
        s.activate(ActivationTarget::Project(b.clone()));
        let out = s.activate(ActivationTarget::Tab { project: a.clone() });
        assert_eq!(out.current.project.as_ref(), Some(&a));
        assert_eq!(out.current.task_id, Some(7), "恢复 A 最近选中的 Task");
    }

    #[test]
    fn agent_card_activation_carries_session_read_intent() {
        let a = tmp_id("ag");
        let b = tmp_id("ag2");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: b.clone(),
            task_id: 3,
        });
        let out = s.activate(ActivationTarget::AgentRun {
            project: a.clone(),
            task_id: Some(9),
            session_id: 42,
        });
        assert_eq!(out.current.project.as_ref(), Some(&a));
        assert_eq!(out.current.task_id, Some(9));
        assert_eq!(out.mark_session_read, Some((a.clone(), 42)));
        // 可解析 Task 时同步为该项目最近 Task
        assert_eq!(s.last_task_of(&a), Some(9));
    }

    #[test]
    fn remove_background_project_keeps_context() {
        let a = tmp_id("rma");
        let b = tmp_id("rmb");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: a.clone(),
            task_id: 1,
        });
        s.open_project(b.clone());
        let out = s.remove_project(&b);
        assert!(!out.project_changed);
        assert_eq!(s.snapshot().project.as_ref(), Some(&a));
    }

    #[test]
    fn remove_current_project_falls_back_to_most_recent() {
        let a = tmp_id("fba");
        let b = tmp_id("fbb");
        let c = tmp_id("fbc");
        let mut s = ProjectContextState::new();
        // 激活顺序 A → B → C → B
        for p in [&a, &b, &c] {
            s.open_project(p.clone());
        }
        s.activate(ActivationTarget::Project(a.clone()));
        s.activate(ActivationTarget::Task {
            project: b.clone(),
            task_id: 2,
        });
        s.activate(ActivationTarget::Project(c.clone()));
        s.activate(ActivationTarget::Project(b.clone()));
        let out = s.remove_project(&b);
        assert_eq!(out.current.project.as_ref(), Some(&c), "回退到最近激活的 C");
        assert!(out.project_changed);
    }

    #[test]
    fn remove_last_project_clears_context() {
        let a = tmp_id("last");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: a.clone(),
            task_id: 1,
        });
        let out = s.remove_project(&a);
        assert_eq!(out.current, ActiveProjectContext::default());
        assert!(out.project_changed);
    }

    #[test]
    fn remove_only_activated_project_falls_back_to_registered_project() {
        let a = tmp_id("registered-a");
        let b = tmp_id("registered-b");
        let mut s = ProjectContextState::new();
        s.open_project(a.clone());
        s.open_project(b.clone());
        s.activate(ActivationTarget::Project(b.clone()));

        let out = s.remove_project(&b);

        assert_eq!(
            out.current.project.as_ref(),
            Some(&a),
            "仍有已注册项目时不能落到无项目状态"
        );
    }

    #[test]
    fn alias_paths_share_one_identity() {
        let dir = std::env::temp_dir().join(format!("mf-pctx-alias-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let direct = normalize_project_path(&dir).0;
        // `dir\.\` 与 `dir\..\basename` 都应规范化为同一身份
        let dotted = normalize_project_path(&dir.join(".")).0;
        let via_parent =
            normalize_project_path(&dir.parent().unwrap().join(dir.file_name().unwrap())).0;
        assert_eq!(direct, dotted);
        assert_eq!(direct, via_parent);
    }

    #[test]
    fn task_gone_clears_selection_residue() {
        let a = tmp_id("gone");
        let mut s = ProjectContextState::new();
        s.activate(ActivationTarget::Task {
            project: a.clone(),
            task_id: 11,
        });
        s.task_gone(&a, 11);
        assert_eq!(s.last_task_of(&a), None);
        assert_eq!(s.snapshot().task_id, None);
        // 再次 Tab 激活不复活已删除的 Task
        let out = s.activate(ActivationTarget::Tab { project: a.clone() });
        assert_eq!(out.current.task_id, None);
    }

    #[test]
    fn no_verbatim_prefix_in_display() {
        let a = pid(r"C:\definitely\not\here");
        assert!(!a.as_path().display().to_string().contains(r"\\?\"));
    }

    #[test]
    fn nested_project_owns_its_files_over_parent_project() {
        let root = std::env::temp_dir().join(format!("mf-pctx-nested-{}", std::process::id()));
        let child = root.join("child");
        let file = child.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        let parent = normalize_project_path(&root).0;
        let child = normalize_project_path(&child).0;
        let file = normalize_project_path(&file).0;

        let owner = deepest_owning_project(&[parent, child.clone()], file.as_path());

        assert_eq!(owner, Some(child));
    }
}
