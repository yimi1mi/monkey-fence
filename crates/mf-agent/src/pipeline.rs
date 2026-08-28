//! 流水线领域模型:Pipeline Draft、DAG 校验、拓扑分层与会话策略。
//!
//! 术语见 `CONTEXT.md`:Task 的一级目标是 Task,DAG 节点是 Step,
//! Task 的每个不可变 DAG 版本是 Pipeline Revision。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Step 的会话策略:默认 fresh(每 Step 新会话),或 reuse(复用同名会话键)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionPolicy {
    #[default]
    Fresh,
    Reuse {
        key: String,
    },
}

impl SessionPolicy {
    pub fn as_db_str(&self) -> String {
        match self {
            SessionPolicy::Fresh => "fresh".into(),
            SessionPolicy::Reuse { key } => format!("reuse:{}", key),
        }
    }
    pub fn parse_db(s: &str) -> SessionPolicy {
        if let Some(key) = s.strip_prefix("reuse:") {
            SessionPolicy::Reuse { key: key.into() }
        } else {
            SessionPolicy::Fresh
        }
    }
    pub fn session_key(&self) -> Option<&str> {
        match self {
            SessionPolicy::Fresh => None,
            SessionPolicy::Reuse { key } => Some(key),
        }
    }
}

/// DAG 节点草案(未持久化)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepDraft {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub instructions: String,
    pub agent_profile: String,
    #[serde(default)]
    pub session_policy: SessionPolicy,
    #[serde(default)]
    pub deps: Vec<String>,
}

/// 流水线草案:由模板、Planner 或手工创建,校验通过并经用户确认后激活。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineDraft {
    pub steps: Vec<StepDraft>,
}

/// 校验时可用的 Agent Profile 索引(由插件注册表提供)。
#[derive(Debug, Clone, Default)]
pub struct ProfileIndex {
    /// profile id → (installed, enabled, detected/available)
    pub entries: HashMap<String, ProfileAvailability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileAvailability {
    pub installed: bool,
    pub enabled: bool,
    pub detected: bool,
}

impl ProfileIndex {
    pub fn available(&self, id: &str) -> ProfileAvailability {
        self.entries
            .get(id)
            .copied()
            .unwrap_or(ProfileAvailability {
                installed: false,
                enabled: false,
                detected: false,
            })
    }
    pub fn is_usable(&self, id: &str) -> bool {
        let a = self.available(id);
        a.installed && a.enabled && a.detected
    }
}

impl PipelineDraft {
    pub fn step(&self, key: &str) -> Option<&StepDraft> {
        self.steps.iter().find(|s| s.key == key)
    }

    /// Kahn 分层:返回每层 step key,用于 Pipeline 视图的左到右拓扑列。
    /// 输入必须先通过 [`validate`](Self::validate);若有环,环中节点不出现在结果里。
    pub fn topo_levels(&self) -> Vec<Vec<String>> {
        let keys: HashSet<&str> = self.steps.iter().map(|s| s.key.as_str()).collect();
        let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for s in &self.steps {
            indegree.entry(s.key.as_str()).or_insert(0);
            for d in &s.deps {
                if keys.contains(d.as_str()) {
                    *indegree.entry(s.key.as_str()).or_insert(0) += 1;
                    dependents
                        .entry(d.as_str())
                        .or_default()
                        .push(s.key.as_str());
                }
            }
        }
        let mut levels = Vec::new();
        let mut current: Vec<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        while !current.is_empty() {
            current.sort_unstable();
            levels.push(current.iter().map(|s| s.to_string()).collect());
            let mut next = Vec::new();
            for k in current {
                if let Some(deps) = dependents.get(k) {
                    for &t in deps {
                        if let Some(e) = indegree.get_mut(t) {
                            *e -= 1;
                            if *e == 0 {
                                next.push(t);
                            }
                        }
                    }
                }
            }
            current = next;
        }
        levels
    }

    /// 传递闭包:reachable[from] 包含 from 的全部(传递)后代 key。
    fn reachability(&self) -> HashMap<&str, BTreeSet<&str>> {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        let valid: HashSet<&str> = self.steps.iter().map(|s| s.key.as_str()).collect();
        for s in &self.steps {
            for d in &s.deps {
                if valid.contains(d.as_str()) {
                    adj.entry(d.as_str()).or_default().push(s.key.as_str());
                }
            }
        }
        let mut out = HashMap::new();
        for s in &self.steps {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<&str> = adj.get(s.key.as_str()).cloned().unwrap_or_default();
            while let Some(n) = stack.pop() {
                if seen.insert(n) {
                    if let Some(next) = adj.get(n) {
                        stack.extend(next.iter().copied());
                    }
                }
            }
            out.insert(s.key.as_str(), seen);
        }
        out
    }

    /// 校验草案。返回全部错误(空切片 = 通过):
    /// - Step key 非空、合法(`[A-Za-z0-9_-]`)且唯一
    /// - 依赖存在且不自指
    /// - 无循环依赖
    /// - Agent Profile 已安装、已启用且可用
    /// - 相同 session key 的 Step 在 DAG 中必须两两有序(不能并行)
    pub fn validate(&self, profiles: &ProfileIndex) -> Vec<String> {
        let mut errs = Vec::new();
        let mut seen = HashSet::new();
        for s in &self.steps {
            if s.key.is_empty() {
                errs.push("存在空 Step key".into());
            } else if !s
                .key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                errs.push(format!("Step key 非法(仅允许字母数字/-/_): {}", s.key));
            } else if !seen.insert(s.key.clone()) {
                errs.push(format!("Step key 重复: {}", s.key));
            }
            if s.title.trim().is_empty() {
                errs.push(format!("Step 标题为空: {}", s.key));
            }
            if s.agent_profile.trim().is_empty() {
                errs.push(format!("Step {} 未指派 Agent Profile", s.key));
            } else if !profiles.is_usable(&s.agent_profile) {
                errs.push(format!(
                    "Step {} 的 Agent Profile `{}` 未安装、未启用或不可用",
                    s.key, s.agent_profile
                ));
            }
            if let SessionPolicy::Reuse { key } = &s.session_policy {
                if key.trim().is_empty() {
                    errs.push(format!("Step {} 的 session key 为空", s.key));
                }
            }
        }
        let keys: HashSet<&str> = self.steps.iter().map(|s| s.key.as_str()).collect();
        for s in &self.steps {
            for d in &s.deps {
                if d == &s.key {
                    errs.push(format!("Step {} 依赖自身", s.key));
                } else if !keys.contains(d.as_str()) {
                    errs.push(format!("Step {} 依赖不存在的 Step `{}`", s.key, d));
                }
            }
        }
        // 环检测:DFS 三色标记
        {
            let mut color: HashMap<&str, u8> = HashMap::new(); // 0 white 1 gray 2 black
            fn dfs<'a>(
                node: &'a str,
                adj: &HashMap<&'a str, Vec<&'a str>>,
                color: &mut HashMap<&'a str, u8>,
                errs: &mut Vec<String>,
            ) {
                color.insert(node, 1);
                if let Some(next) = adj.get(node) {
                    for &n in next {
                        match color.get(n).copied().unwrap_or(0) {
                            0 => dfs(n, adj, color, errs),
                            1 => errs.push(format!("检测到循环依赖(涉及 Step `{}`)", n)),
                            _ => {}
                        }
                    }
                }
                color.insert(node, 2);
            }
            let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
            for s in &self.steps {
                for d in &s.deps {
                    if keys.contains(d.as_str()) && d != &s.key {
                        adj.entry(d.as_str()).or_default().push(s.key.as_str());
                    }
                }
            }
            for s in &self.steps {
                if color.get(s.key.as_str()).copied().unwrap_or(0) == 0 {
                    dfs(s.key.as_str(), &adj, &mut color, &mut errs);
                }
            }
        }
        // 会话复用串行约束:同 key 的步骤两两必须有祖先/后代关系
        {
            let mut by_key: HashMap<&str, Vec<&str>> = HashMap::new();
            for s in &self.steps {
                if let SessionPolicy::Reuse { key } = &s.session_policy {
                    by_key.entry(key.as_str()).or_default().push(s.key.as_str());
                }
            }
            if !by_key.is_empty() {
                let reach = self.reachability();
                for (key, steps) in by_key {
                    for i in 0..steps.len() {
                        for j in i + 1..steps.len() {
                            let a = steps[i];
                            let b = steps[j];
                            let ordered = reach.get(a).map(|s| s.contains(b)).unwrap_or(false)
                                || reach.get(b).map(|s| s.contains(a)).unwrap_or(false);
                            if !ordered {
                                errs.push(format!(
                                    "session key `{}` 被 Step {} 与 {} 复用,但两者在 DAG 中无顺序关系(禁止并行复用)",
                                    key, a, b
                                ));
                            }
                        }
                    }
                }
            }
        }
        errs.sort();
        errs.dedup();
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> ProfileIndex {
        let mut p = ProfileIndex::default();
        p.entries.insert(
            "mock".into(),
            ProfileAvailability {
                installed: true,
                enabled: true,
                detected: true,
            },
        );
        p.entries.insert(
            "disabled".into(),
            ProfileAvailability {
                installed: true,
                enabled: false,
                detected: true,
            },
        );
        p.entries.insert(
            "notdetected".into(),
            ProfileAvailability {
                installed: true,
                enabled: true,
                detected: false,
            },
        );
        p
    }

    fn step(key: &str, deps: &[&str]) -> StepDraft {
        StepDraft {
            key: key.into(),
            title: format!("step {key}"),
            instructions: String::new(),
            agent_profile: "mock".into(),
            session_policy: SessionPolicy::Fresh,
            deps: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn valid_dag_passes() {
        let d = PipelineDraft {
            steps: vec![step("a", &[]), step("b", &["a"]), step("c", &["a"])],
        };
        assert!(d.validate(&profiles()).is_empty());
        assert_eq!(d.topo_levels(), vec![vec!["a"], vec!["b", "c"]]);
    }

    #[test]
    fn cycle_rejected() {
        let d = PipelineDraft {
            steps: vec![step("a", &["c"]), step("b", &["a"]), step("c", &["b"])],
        };
        let errs = d.validate(&profiles());
        assert!(errs.iter().any(|e| e.contains("循环依赖")), "{errs:?}");
    }

    #[test]
    fn missing_dep_rejected() {
        let d = PipelineDraft {
            steps: vec![step("a", &["ghost"])],
        };
        let errs = d.validate(&profiles());
        assert!(errs.iter().any(|e| e.contains("不存在")), "{errs:?}");
    }

    #[test]
    fn duplicate_key_rejected() {
        let d = PipelineDraft {
            steps: vec![step("a", &[]), step("a", &[])],
        };
        let errs = d.validate(&profiles());
        assert!(errs.iter().any(|e| e.contains("重复")), "{errs:?}");
    }

    #[test]
    fn profile_states_rejected() {
        let mut s = step("a", &[]);
        s.agent_profile = "unknown".into();
        assert!(PipelineDraft {
            steps: vec![s.clone()]
        }
        .validate(&profiles())[0]
            .contains("未安装"));
        s.agent_profile = "disabled".into();
        assert!(PipelineDraft {
            steps: vec![s.clone()]
        }
        .validate(&profiles())[0]
            .contains("未启用"));
        s.agent_profile = "notdetected".into();
        assert!(PipelineDraft { steps: vec![s] }.validate(&profiles())[0].contains("不可用"));
    }

    #[test]
    fn parallel_reuse_rejected_but_ordered_allowed() {
        // a → b, a → c,b/c 同 session key:并行 → 拒绝
        let mut b = step("b", &["a"]);
        b.session_policy = SessionPolicy::Reuse { key: "s".into() };
        let mut c = step("c", &["a"]);
        c.session_policy = SessionPolicy::Reuse { key: "s".into() };
        let d = PipelineDraft {
            steps: vec![step("a", &[]), b, c],
        };
        assert!(d
            .validate(&profiles())
            .iter()
            .any(|e| e.contains("禁止并行复用")));

        // a → b → c 同 key:有序 → 通过
        let mut b = step("b", &["a"]);
        b.session_policy = SessionPolicy::Reuse { key: "s".into() };
        let mut c = step("c", &["b"]);
        c.session_policy = SessionPolicy::Reuse { key: "s".into() };
        let d = PipelineDraft {
            steps: vec![step("a", &[]), b, c],
        };
        assert!(
            d.validate(&profiles()).is_empty(),
            "{:?}",
            d.validate(&profiles())
        );
    }

    #[test]
    fn session_policy_db_roundtrip() {
        assert_eq!(SessionPolicy::parse_db("fresh"), SessionPolicy::Fresh);
        assert_eq!(
            SessionPolicy::parse_db("reuse:build"),
            SessionPolicy::Reuse {
                key: "build".into()
            }
        );
        assert_eq!(SessionPolicy::Fresh.as_db_str(), "fresh");
    }

    #[test]
    fn json_roundtrip() {
        let d = PipelineDraft {
            steps: vec![step("a", &[])],
        };
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains("\"kind\":\"fresh\""));
        let back: PipelineDraft = serde_json::from_str(&j).unwrap();
        assert_eq!(back, d);
    }
}
