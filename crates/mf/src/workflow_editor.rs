//! Workflow Editor 状态模型(UI 计划 Task 2;设计 §11.2)。
//!
//! 纯状态:布局偏好(默认 B:左侧实例库 + 中间画布 + 右侧检查器,
//! 可切换 A 上下布局并记住)、画布模型(拖入/连线/环拒绝/拓扑自动
//! 分层/选中/删除)、编辑器侧诊断。GPUI 渲染在 workflow_canvas。

use std::collections::HashSet;

/// 编辑器布局:默认 B(侧栏),可切换 A(上下堆叠)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowLayout {
    /// 布局 B:左侧实例库 + 中间 DAG 画布 + 右侧检查器(默认)。
    Sidebar,
    /// 布局 A:上下堆叠(画布在上,库/检查器在下)。
    Stacked,
}

/// 布局偏好存取(注入便于测试:内存实现 + 文件实现)。
pub trait EditorPrefs {
    fn layout(&self) -> Option<WorkflowLayout>;
    fn save_layout(&mut self, layout: WorkflowLayout);
}

/// 内存偏好(测试)。
#[derive(Default)]
pub struct MemoryPrefs {
    layout: Option<WorkflowLayout>,
}

/// 文件偏好:`~/.monkeyfence/ui-prefs.json`(布局 A/B 记忆;设计 §11.2)。
pub struct FilePrefs {
    path: std::path::PathBuf,
}

impl FilePrefs {
    pub fn default_path() -> FilePrefs {
        FilePrefs {
            path: dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".monkeyfence")
                .join("ui-prefs.json"),
        }
    }

    fn read_json(&self) -> serde_json::Value {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::json!({}))
    }
}

impl EditorPrefs for FilePrefs {
    fn layout(&self) -> Option<WorkflowLayout> {
        match self.read_json().get("workflow_layout")?.as_str()? {
            "stacked" => Some(WorkflowLayout::Stacked),
            "sidebar" => Some(WorkflowLayout::Sidebar),
            _ => Some(WorkflowLayout::Sidebar),
        }
    }

    fn save_layout(&mut self, layout: WorkflowLayout) {
        let mut json = self.read_json();
        json["workflow_layout"] = serde_json::json!(match layout {
            WorkflowLayout::Sidebar => "sidebar",
            WorkflowLayout::Stacked => "stacked",
        });
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &self.path,
            serde_json::to_vec_pretty(&json).unwrap_or_default(),
        );
    }
}

impl EditorPrefs for MemoryPrefs {
    fn layout(&self) -> Option<WorkflowLayout> {
        self.layout
    }
    fn save_layout(&mut self, layout: WorkflowLayout) {
        self.layout = Some(layout);
    }
}

/// 画布节点(编辑期):引用 Agent Instance id 或 `default-cli:<完整贡献 ID>`
/// 保留引用(不引用 Profile)。
#[derive(Debug, Clone, PartialEq)]
pub struct EditorNode {
    pub key: String,
    pub title: String,
    pub instance_id: String,
    pub deps: Vec<String>,
    /// 节点工作说明(保存草稿时随图持久化)。
    pub instructions: String,
}

/// 生成不与 `existing` 冲突的稳定项目工作流 key(wf-1、wf-2 …;
/// 取第一个空闲编号,删除后可复用空洞)。
/// key 只在此处生成,不使用 task id(ADR 0004)。
pub fn next_workflow_key(existing: &[String]) -> String {
    let taken: HashSet<&str> = existing.iter().map(|s| s.as_str()).collect();
    let mut n = 1usize;
    while taken.contains(format!("wf-{n}").as_str()) {
        n += 1;
    }
    format!("wf-{n}")
}

/// 编辑器状态(纯逻辑)。
pub struct WorkflowEditorState {
    layout: WorkflowLayout,
    nodes: Vec<EditorNode>,
    selected: Option<String>,
    counter: usize,
}

impl WorkflowEditorState {
    pub fn load(prefs: &dyn EditorPrefs) -> WorkflowEditorState {
        WorkflowEditorState {
            layout: prefs.layout().unwrap_or(WorkflowLayout::Sidebar),
            nodes: Vec::new(),
            selected: None,
            counter: 0,
        }
    }

    pub fn layout(&self) -> WorkflowLayout {
        self.layout
    }

    pub fn set_layout(&mut self, layout: WorkflowLayout, prefs: &mut dyn EditorPrefs) {
        self.layout = layout;
        prefs.save_layout(layout);
    }

    /// 整体替换节点(任务切换时加载草稿);计数器重置为节点数。
    pub fn load_nodes(&mut self, nodes: Vec<EditorNode>) {
        self.counter = nodes.len();
        self.selected = None;
        self.nodes = nodes;
    }

    pub fn nodes(&self) -> &[EditorNode] {
        &self.nodes
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn select(&mut self, key: &str) {
        if self.nodes.iter().any(|n| n.key == key) {
            self.selected = Some(key.to_string());
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// 修改选中节点标题(检查器输入)。
    pub fn set_selected_title(&mut self, title: &str) {
        if let Some(key) = self.selected.clone() {
            if let Some(node) = self.nodes.iter_mut().find(|n| n.key == key) {
                node.title = title.to_string();
            }
        }
    }

    /// 修改选中节点工作说明(检查器输入;原子编辑动作)。
    pub fn set_selected_instructions(&mut self, instructions: &str) {
        if let Some(key) = self.selected.clone() {
            if let Some(node) = self.nodes.iter_mut().find(|n| n.key == key) {
                node.instructions = instructions.to_string();
            }
        }
    }

    /// 修改选中节点的 Agent 绑定(默认 CLI 引用或保存实例 id;原子编辑动作)。
    pub fn set_selected_instance(&mut self, reference: &str) {
        if let Some(key) = self.selected.clone() {
            if let Some(node) = self.nodes.iter_mut().find(|n| n.key == key) {
                node.instance_id = reference.to_string();
            }
        }
    }

    /// 从实例库拖入画布:生成唯一节点键(step-1、step-2 …)。
    pub fn drag_from_library(&mut self, instance_id: &str) {
        self.counter += 1;
        self.nodes.push(EditorNode {
            instructions: String::new(),
            key: format!("step-{}", self.counter),
            title: format!("步骤 {}", self.counter),
            instance_id: instance_id.to_string(),
            deps: Vec::new(),
        });
    }

    /// 添加依赖(画布连线);环与自依赖拒绝。
    pub fn add_dependency(&mut self, node: &str, dep: &str) -> Result<(), String> {
        if node == dep {
            return Err("不允许自依赖".into());
        }
        if !self.nodes.iter().any(|n| n.key == node) || !self.nodes.iter().any(|n| n.key == dep) {
            return Err("节点不存在".into());
        }
        // 环检测:dep 能否到达 node(能则 node→dep 成环)
        if self.reaches(dep, node) {
            return Err(format!("添加依赖 {node} → {dep} 会形成环"));
        }
        if let Some(n) = self.nodes.iter_mut().find(|n| n.key == node) {
            if !n.deps.iter().any(|d| d == dep) {
                n.deps.push(dep.to_string());
            }
        }
        Ok(())
    }

    pub fn remove_dependency(&mut self, node: &str, dep: &str) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.key == node) {
            n.deps.retain(|d| d != dep);
        }
    }

    /// 删除选中节点并清理悬空依赖。
    pub fn delete_selected(&mut self) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        self.nodes.retain(|n| n.key != key);
        for node in &mut self.nodes {
            node.deps.retain(|d| d != &key);
        }
        self.selected = None;
    }

    fn reaches(&self, from: &str, to: &str) -> bool {
        let mut stack = vec![from];
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(current) = stack.pop() {
            if current == to {
                return true;
            }
            if !seen.insert(current) {
                continue;
            }
            if let Some(node) = self.nodes.iter().find(|n| n.key == current) {
                for dep in &node.deps {
                    stack.push(dep.as_str());
                }
            }
        }
        false
    }

    /// 拓扑自动分层:每层节点的依赖全部在更早层。
    /// 返回 (层索引 → 节点键列表);层内按稳定键排序。
    pub fn autolayout(&self) -> Vec<Vec<(String, usize)>> {
        let keys: HashSet<&str> = self.nodes.iter().map(|n| n.key.as_str()).collect();
        let mut placed: HashSet<String> = HashSet::new();
        let mut layers: Vec<Vec<(String, usize)>> = Vec::new();
        let remaining: Vec<(usize, &EditorNode)> = self.nodes.iter().enumerate().collect();
        let mut pending = remaining;
        while !pending.is_empty() {
            let mut layer: Vec<(usize, &EditorNode)> = pending
                .iter()
                .copied()
                .filter(|(_, n)| {
                    n.deps
                        .iter()
                        .all(|d| placed.contains(d) || !keys.contains(d.as_str()))
                })
                .collect();
            if layer.is_empty() {
                break; // 环(编辑器已拒绝;防御)
            }
            layer.sort_by_key(|(_, n)| n.key.clone());
            let rendered: Vec<(String, usize)> =
                layer.iter().map(|(i, n)| (n.key.clone(), *i)).collect();
            for (key, _) in &rendered {
                placed.insert(key.clone());
            }
            layers.push(rendered);
            let in_layer: HashSet<usize> = layer.iter().map(|(i, _)| *i).collect();
            pending.retain(|(i, _)| !in_layer.contains(i));
        }
        layers
    }

    /// 编辑器侧最小诊断(完整编译诊断在保存/运行前由 Workflow Compiler 提供)。
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.nodes.is_empty() {
            out.push("工作流至少需要一个节点(从左侧实例库拖入)".into());
        }
        out
    }
}
