use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// MonkeyFence 技能:声明式提示增强 + 工具白名单
/// 一个技能 = 一个目录:skill.toml(元数据) + INSTRUCTIONS.md(注入正文)
#[derive(Clone, Debug, Deserialize)]
pub struct SkillMeta {
    pub id: String,
    pub title: String,
    /// 任务说明中出现这些词时激活(不区分大小写的词匹配)
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 激活时允许的工具;空 = 全部允许
    #[serde(default)]
    pub tools_allow: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub meta: SkillMeta,
    /// INSTRUCTIONS.md 正文
    pub body: String,
    /// 技能目录(用于展示来源)
    pub source: PathBuf,
}

impl Skill {
    pub fn matches(&self, task_spec: &str) -> bool {
        let spec_lower = task_spec.to_lowercase();
        self.meta.triggers.iter().any(|t| {
            let t = t.to_lowercase();
            // 词边界近似:包含即可(触发词一般足够特异)
            spec_lower.contains(&t)
        })
    }

    pub fn allows_tool(&self, tool: &str) -> bool {
        self.meta.tools_allow.is_empty() || self.meta.tools_allow.iter().any(|t| t == tool)
    }
}

/// 扫描技能目录:项目级 <root>/.monkeyfence/skills 优先于全局 ~/.monkeyfence/skills
/// 同 id 时项目级覆盖全局
pub fn load_skills(project_root: Option<&Path>) -> Vec<Skill> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(root) = project_root {
        dirs.push(root.join(".monkeyfence").join("skills"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".monkeyfence").join("skills"));
    }
    let mut by_id: std::collections::HashMap<String, Skill> = std::collections::HashMap::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for skill_dir in flatten_skill_dirs(&dir) {
            if let Ok(skill) = load_one(&skill_dir) {
                by_id.insert(skill.meta.id.clone(), skill);
            }
        }
    }
    by_id.into_values().collect()
}

fn flatten_skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && e.path().join("skill.toml").is_file()
            {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn load_one(dir: &Path) -> Result<Skill> {
    let meta_text = std::fs::read_to_string(dir.join("skill.toml"))
        .with_context(|| format!("read {}", dir.join("skill.toml").display()))?;
    let meta: SkillMeta = toml::from_str(&meta_text)
        .with_context(|| format!("parse {}", dir.join("skill.toml").display()))?;
    let body_path = dir.join("INSTRUCTIONS.md");
    let body = std::fs::read_to_string(&body_path)
        .with_context(|| format!("read {}", body_path.display()))?;
    Ok(Skill {
        meta,
        body,
        source: dir.to_path_buf(),
    })
}

/// 从任务说明挑选匹配的技能(全部命中者都注入,由调用方限量)
pub fn match_skills<'a>(skills: &'a [Skill], task_spec: &str) -> Vec<&'a Skill> {
    skills
        .iter()
        .filter(|s| s.matches(task_spec))
        .filter(|s| !s.body.trim().is_empty())
        .collect()
}

/// 首次运行时写入内置技能(可由用户编辑/删除)
pub fn seed_builtin_skills(project_root: Option<&Path>) -> Result<()> {
    let base = match project_root {
        Some(root) => root.join(".monkeyfence").join("skills"),
        None => match dirs::home_dir() {
            Some(h) => h.join(".monkeyfence").join("skills"),
            None => return Ok(()),
        },
    };
    let builtins: &[(&str, &str, &[&str], &str)] = &[
        (
            "read-before-edit",
            "先读后写纪律",
            &["修改", "编辑", "重构", "edit", "refactor", "modify", "patch"],
            r#"# 先读后写

修改任何文件之前,必须先用 fs_read 读过当前内容。

- 禁止在未读文件的情况下 fs_write 整个文件。
- fs_patch 的 find 必须从 fs_read 的原文中复制,保证唯一匹配。
- 修改后复查:再次 fs_read 修改区域确认结果符合预期。
"#,
        ),
        (
            "rust-tdd",
            "Rust 红绿重构",
            &["测试", "tdd", "test", "单测", "红绿"],
            r#"# Rust 红绿重构

按红-绿-重构循环推进:

1. 红:先写一个失败的最小测试(cargo test 确认失败)。
2. 绿:写最少实现让测试通过。
3. 重构:在绿灯下清理代码,保持测试通过。

每轮循环后运行 `cargo test`,不得跳过。
"#,
        ),
        (
            "p4-safe-submit",
            "P4 安全提交",
            &["提交", "submit", "p4", "changelist", "变更列表"],
            r#"# P4 安全提交

提交变更列表前:

1. `p4 opened` 确认没有无关文件混入。
2. 检查 diff 只包含目标改动。
3. 描述遵循团队规范(做了什么/为什么/影响面)。
4. 不确定时用 ask_human 让用户确认文件清单。
"#,
        ),
    ];
    for (id, title, triggers, body) in builtins {
        let dir = base.join(id);
        if dir.join("skill.toml").exists() {
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        let triggers_list = triggers
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.join("skill.toml"),
            format!(
                "id = \"{}\"\ntitle = \"{}\"\ntriggers = [{}]\n",
                id, title, triggers_list
            ),
        )?;
        std::fs::write(dir.join("INSTRUCTIONS.md"), body)?;
    }
    Ok(())
}

/// 工具白名单汇总:多个技能命中时取交集;空 = 全部
pub fn allowed_tools(skills: &[&Skill], all_tools: &[&str]) -> HashSet<String> {
    let mut allow: Option<HashSet<String>> = None;
    for s in skills {
        if s.meta.tools_allow.is_empty() {
            continue;
        }
        let set: HashSet<String> = s.meta.tools_allow.iter().cloned().collect();
        allow = Some(match allow {
            None => set,
            Some(prev) => prev.intersection(&set).cloned().collect(),
        });
    }
    match allow {
        Some(set) => set,
        None => all_tools.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, id: &str, triggers: &[&str], tools: &[&str], body: &str) {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        let tr = triggers.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
        let tl = tools.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
        std::fs::write(
            d.join("skill.toml"),
            format!("id = \"{id}\"\ntitle = \"t\"\ntriggers = [{tr}]\ntools_allow = [{tl}]\n"),
        )
        .unwrap();
        std::fs::write(d.join("INSTRUCTIONS.md"), body).unwrap();
    }

    #[test]
    fn load_and_match() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".monkeyfence").join("skills");
        write_skill(&skills_dir, "s1", &["refactor", "重构"], &[], "# 技能一\n内容");
        write_skill(&skills_dir, "s2", &["测试"], &["fs_read"], "# 技能二\n内容");
        let skills = load_skills(Some(tmp.path()));
        assert_eq!(skills.len(), 2, "loaded: {:?}", skills.iter().map(|s| &s.meta.id).collect::<Vec<_>>());

        let hits = match_skills(&skills, "请重构这段代码");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].meta.id, "s1");

        let hits2 = match_skills(&skills, "为模块补充测试");
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].meta.id, "s2");
        assert!(!hits2[0].allows_tool("fs_write"));
        assert!(hits2[0].allows_tool("fs_read"));
    }

    #[test]
    fn seed_writes_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        seed_builtin_skills(Some(tmp.path())).unwrap();
        let skills = load_skills(Some(tmp.path()));
        assert!(skills.len() >= 3, "builtins: {}", skills.len());
        assert!(skills.iter().any(|s| s.meta.id == "read-before-edit"));
        // 幂等
        seed_builtin_skills(Some(tmp.path())).unwrap();
        assert_eq!(load_skills(Some(tmp.path())).len(), skills.len());
    }
}
