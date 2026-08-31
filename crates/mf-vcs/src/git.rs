use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Git 基础操作(git2 实现):状态/暂存/提交/日志/diff
pub struct Git {
    repo: git2::Repository,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitStatus {
    New,
    Modified,
    Deleted,
    Renamed,
    Staged { kind: Box<GitStatus> },
}

impl GitStatus {
    pub fn label(&self) -> &'static str {
        match self {
            GitStatus::New => "新增",
            GitStatus::Modified => "修改",
            GitStatus::Deleted => "删除",
            GitStatus::Renamed => "重命名",
            GitStatus::Staged { .. } => "已暂存",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitFileEntry {
    pub path: PathBuf, // 相对仓库根
    pub status: GitStatus,
}

#[derive(Clone, Debug)]
pub struct GitLogEntry {
    pub id: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
}

impl Git {
    /// 初始化新仓库(测试与隔离目录场景;工作目录等同 open)。
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let repo = git2::Repository::init(root)
            .with_context(|| format!("初始化 git 仓库失败: {}", root.display()))?;
        Ok(Self { repo })
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let repo = git2::Repository::discover(root)
            .with_context(|| format!("发现 git 仓库失败: {}", root.display()))?;
        Ok(Self { repo })
    }

    pub fn is_repo(root: impl AsRef<Path>) -> bool {
        git2::Repository::discover(root).is_ok()
    }

    pub fn branch(&self) -> Result<String> {
        let head = self.repo.head().context("读取 HEAD")?;
        if head.is_branch() {
            Ok(head.shorthand().unwrap_or("HEAD").to_string())
        } else {
            Ok(head
                .target()
                .map(|t| t.to_string()[..8.min(8)].to_string())
                .unwrap_or_default())
        }
    }

    pub fn status(&self) -> Result<Vec<GitFileEntry>> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(false);
        let statuses = self.repo.statuses(Some(&mut opts)).context("git status")?;
        let mut out = Vec::new();
        for e in statuses.iter() {
            let s = e.status();
            let path = e.path().map(PathBuf::from);
            let (Some(path), kind) = (path, s) else {
                continue;
            };
            let entry_status = if s.is_index_new() {
                GitStatus::Staged {
                    kind: Box::new(GitStatus::New),
                }
            } else if s.is_index_deleted() {
                GitStatus::Staged {
                    kind: Box::new(GitStatus::Deleted),
                }
            } else if s.is_index_renamed() {
                GitStatus::Staged {
                    kind: Box::new(GitStatus::Renamed),
                }
            } else if s.is_index_modified() {
                GitStatus::Staged {
                    kind: Box::new(GitStatus::Modified),
                }
            } else if s.is_wt_new() {
                GitStatus::New
            } else if s.is_wt_deleted() {
                GitStatus::Deleted
            } else if s.is_wt_renamed() {
                GitStatus::Renamed
            } else {
                GitStatus::Modified
            };
            out.push(GitFileEntry {
                path,
                status: entry_status,
            });
        }
        Ok(out)
    }

    pub fn stage(&self, paths: &[PathBuf]) -> Result<()> {
        let mut index = self.repo.index()?;
        for p in paths {
            index
                .add_path(p)
                .with_context(|| format!("暂存 {}", p.display()))?;
        }
        index.write()?;
        Ok(())
    }

    pub fn unstage(&self, paths: &[PathBuf]) -> Result<()> {
        let head = self.repo.head().and_then(|h| h.peel_to_commit()).ok();
        let mut index = self.repo.index()?;
        for p in paths {
            match &head {
                Some(commit) => {
                    let tree = commit.tree()?;
                    if let Ok(entry) = tree.get_path(p) {
                        index.add_frombuffer(
                            &git2::IndexEntry {
                                ctime: git2::IndexTime::new(0, 0),
                                mtime: git2::IndexTime::new(0, 0),
                                dev: 0,
                                ino: 0,
                                mode: entry.filemode() as u32,
                                uid: 0,
                                gid: 0,
                                file_size: 0,
                                id: entry.id(),
                                flags: 0,
                                flags_extended: 0,
                                path: p.to_string_lossy().into_owned().into_bytes(),
                            },
                            &[],
                        )?;
                    } else {
                        index.remove_path(p).ok();
                    }
                }
                None => {
                    index.remove_path(p).ok();
                }
            }
        }
        index.write()?;
        Ok(())
    }

    pub fn commit(&self, message: &str) -> Result<String> {
        let sig = self
            .repo
            .signature()
            .or_else(|_| git2::Signature::now("MonkeyFence", "monkeyfence@local"))?;
        let mut index = self.repo.index()?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(h) => h.peel_to_commit().map(|c| vec![c]).unwrap_or_default(),
            Err(_) => vec![],
        };
        let parents_ref: Vec<&git2::Commit> = parents.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents_ref)?;
        Ok(oid.to_string())
    }

    pub fn log(&self, max: usize) -> Result<Vec<GitLogEntry>> {
        self.log_page(0, max)
    }

    pub fn log_page(&self, skip: usize, max: usize) -> Result<Vec<GitLogEntry>> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        let mut out = Vec::new();
        for oid in revwalk.skip(skip).take(max) {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            out.push(GitLogEntry {
                id: oid.to_string()[..8].to_string(),
                summary: commit.summary().unwrap_or_default().to_string(),
                author: commit.author().name().unwrap_or("").to_string(),
                time: commit.time().seconds(),
            });
        }
        Ok(out)
    }

    /// 工作区 vs HEAD 的单文件 diff(unified)
    pub fn diff_file(&self, rel_path: &Path) -> Result<String> {
        let head_tree = self.repo.head().and_then(|h| h.peel_to_tree()).ok();
        let mut diff_opts = git2::DiffOptions::new();
        diff_opts
            .pathspec(rel_path.to_string_lossy().into_owned())
            .context_lines(3);
        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut diff_opts))?;
        let mut text = String::new();
        diff.print(git2::DiffFormat::Patch, |_d, _h, line| {
            match line.origin() {
                '+' | '-' | ' ' => text.push(line.origin()),
                _ => {}
            }
            if let Ok(content) = std::str::from_utf8(line.content()) {
                text.push_str(content.trim_end_matches('\n'));
            }
            text.push('\n');
            true
        })?;
        Ok(text)
    }

    pub fn root(&self) -> PathBuf {
        self.repo
            .workdir()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }

    // ---------- worktree 管理(卡片墙/驾驶舱用) ----------

    /// worktree 统一根:`<仓库根>/../.worktrees`(所有 mf 派生 worktree
    /// 都必须位于其下,清理前据此校验路径)。
    pub fn worktree_root(&self) -> Result<PathBuf> {
        let root = self.root();
        let parent = root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("仓库根没有父目录"))?;
        Ok(parent.join(".worktrees"))
    }

    /// 列出全部 worktree (name, 绝对路径)
    pub fn worktree_list(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        for name in self.repo.worktrees()?.iter().flatten() {
            if let Ok(wt) = self.repo.find_worktree(name) {
                if let Some(p) = wt.path().to_str() {
                    out.push((name.to_string(), PathBuf::from(p)));
                }
            }
        }
        Ok(out)
    }

    /// 在 `<root>/../.worktrees/<name>` 创建 worktree + 同名新分支(基于当前 HEAD)
    pub fn worktree_create(&self, name: &str) -> Result<PathBuf> {
        self.worktree_create_at(name, None)
    }

    /// 在指定基线提交创建 worktree(基线 None = 当前 HEAD)。
    /// worktree 隔离的 Task/Revision 集成基线由此进入:下游 worktree
    /// 从汇合结果(而非仓库 HEAD)检出,串行下游天然看见上游修改。
    pub fn worktree_create_at(&self, name: &str, baseline: Option<git2::Oid>) -> Result<PathBuf> {
        let parent = self
            .root()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("仓库根没有父目录"))?
            .to_path_buf();
        let dir = parent.join(".worktrees").join(name);
        anyhow::ensure!(!dir.exists(), "worktree 目录已存在: {}", dir.display());
        std::fs::create_dir_all(dir.parent().unwrap_or(&parent))?;
        let base = match baseline {
            Some(oid) => oid,
            None => self.repo.head()?.peel_to_commit()?.id(),
        };
        let base_commit = self.repo.find_commit(base)?;
        let branch_ref = format!("refs/heads/{}", name);
        let mut opts = git2::WorktreeAddOptions::new();
        let reference = self
            .repo
            .reference(&branch_ref, base_commit.id(), false, "mf worktree create")
            .with_context(|| format!("创建分支失败: {}", branch_ref))?;
        opts.reference(Some(&reference));
        let wt = self.repo.worktree(name, &dir, Some(&opts))?;
        let _ = wt;
        Ok(dir)
    }

    // ---------- Task/Revision 集成基线(hidden ref;worktree 提供器维护) ----------

    /// 集成基线 ref 名:`refs/mf/integration/task-<t>-rev-<r>`。
    /// 不在 refs/heads 下,普通分支列表/工具不显示;任务终态时整体删除。
    pub fn integration_ref(task_id: i64, revision_id: i64) -> String {
        format!("refs/mf/integration/task-{task_id}-rev-{revision_id}")
    }

    /// 读取 ref 目标(ref 不存在为 None)。
    pub fn read_ref(&self, refname: &str) -> Result<Option<git2::Oid>> {
        Ok(self
            .repo
            .find_reference(refname)
            .ok()
            .and_then(|r| r.target()))
    }

    /// 把 tree 提交到集成基线 ref 上并前移 ref(父提交 = ref 当前目标,
    /// 无 ref 时为 HEAD)。返回新提交 oid。
    /// 注意:非 CAS 版本,仅供内部/测试使用;生产合并走
    /// [`Git::advance_integration_ref_cas`]。
    pub fn advance_integration_ref(
        &self,
        refname: &str,
        tree_id: git2::Oid,
        message: &str,
    ) -> Result<git2::Oid> {
        let sig = self
            .repo
            .signature()
            .or_else(|_| git2::Signature::now("MonkeyFence", "monkeyfence@local"))?;
        let parent = match self.read_ref(refname)? {
            Some(oid) => vec![self.repo.find_commit(oid)?],
            None => match self.repo.head() {
                Ok(h) => vec![h.peel_to_commit()?],
                Err(_) => Vec::new(),
            },
        };
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let tree = self.repo.find_tree(tree_id)?;
        let oid = self
            .repo
            .commit(Some(refname), &sig, &sig, message, &tree, &parents)?;
        Ok(oid)
    }

    /// 前移集成基线 ref(expected-old CAS,C1):提交对象基于
    /// `expected_old`(None 时基于 HEAD/空)构建,ref 仅当当前目标仍为
    /// `expected_old`(或 ref 不存在)时才更新 —— 并发合并不得双推进,
    /// CAS 失败如实报错(调用方按冲突处理)。
    pub fn advance_integration_ref_cas(
        &self,
        refname: &str,
        tree_id: git2::Oid,
        message: &str,
        expected_old: Option<git2::Oid>,
    ) -> Result<git2::Oid> {
        let sig = self
            .repo
            .signature()
            .or_else(|_| git2::Signature::now("MonkeyFence", "monkeyfence@local"))?;
        let parents: Vec<git2::Commit> = match expected_old {
            Some(oid) => vec![self
                .repo
                .find_commit(oid)
                .with_context(|| format!("expected-old 提交不存在: {oid}"))?],
            None => match self.repo.head() {
                Ok(h) => vec![h.peel_to_commit()?],
                Err(_) => Vec::new(),
            },
        };
        let tree = self.repo.find_tree(tree_id)?;
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        // 只建提交对象,不动 ref;ref 前移单独 CAS
        let commit_oid = self
            .repo
            .commit(None, &sig, &sig, message, &tree, &parent_refs)?;
        let cas_old = expected_old.unwrap_or_else(git2::Oid::zero);
        // force=true + old_id 才是 CAS 形式:force 只绕过"已存在即 EEXISTS"
        // 的预检查,cmp_old_ref 仍强制当前目标 == old_id(不匹配 EMODIFIED;
        // zero oid = ref 必须不存在)。
        self.repo
            .reference_matching(refname, commit_oid, true, cas_old, message)
            .with_context(|| {
                format!(
                    "集成 ref {refname} CAS 失败(已被并发推进,本次合并不得应用): \
                     期望 {:?},当前 {:?}",
                    expected_old,
                    self.read_ref(refname).unwrap_or(None)
                )
            })?;
        Ok(commit_oid)
    }

    /// 把集成基线 ref 指回给定提交(合并应用失败的整体回滚用)。
    /// `target = None` 表示合并前 ref 不存在 → 删除该 ref。
    /// `expect_current`:回滚前 ref 必须处于的值(本次合并推进到的提交);
    /// 不匹配(被并发改动)时拒绝回滚并报错,绝不盲目覆盖。
    /// 幂等:ref 已处于目标状态时 no-op。
    pub fn reset_integration_ref(
        &self,
        refname: &str,
        target: Option<git2::Oid>,
        expect_current: Option<git2::Oid>,
    ) -> Result<()> {
        match target {
            Some(oid) => {
                let commit = self.repo.find_commit(oid)?;
                if self.read_ref(refname)? == Some(oid) {
                    return Ok(());
                }
                match expect_current {
                    Some(cur) => {
                        // force=true + old_id = CAS:当前目标必须仍是 cur
                        self.repo
                            .reference_matching(refname, commit.id(), true, cur, "mf: rollback integration ref")
                            .with_context(|| {
                                format!(
                                    "回滚集成 ref {refname} 失败:已被并发改动(期望 {cur},当前 {:?})",
                                    self.read_ref(refname).unwrap_or(None)
                                )
                            })?;
                    }
                    None => {
                        self.repo.reference(
                            refname,
                            commit.id(),
                            true,
                            "mf: rollback integration ref",
                        )?;
                    }
                }
            }
            None => {
                if let Some(mut reference) = self.repo.find_reference(refname).ok() {
                    if let Some(cur) = expect_current {
                        anyhow::ensure!(
                            reference.target() == Some(cur),
                            "回滚删除集成 ref {refname} 失败:已被并发改动(期望 {cur},当前 {:?})",
                            reference.target()
                        );
                    }
                    reference.delete()?;
                }
            }
        }
        Ok(())
    }

    /// 删除任务全部集成基线 ref(任务终态清理);返回删除数。
    pub fn delete_integration_refs(&self, task_id: i64) -> Result<usize> {
        let prefix = format!("refs/mf/integration/task-{task_id}-");
        let mut removed = 0;
        let refs: Vec<String> = self
            .repo
            .references_glob("refs/mf/integration/*")?
            .filter_map(|r| r.ok())
            .filter_map(|r| r.name().map(str::to_string))
            .collect();
        for name in refs {
            if name.starts_with(&prefix) {
                if let Some(mut r) = self.repo.find_reference(&name).ok() {
                    r.delete()?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// 删除 worktree(prune 元数据 + 移除目录 + 删除同名分支)
    pub fn worktree_remove(&self, name: &str) -> Result<()> {
        let wt = self.repo.find_worktree(name).context("worktree 不存在")?;
        if let Some(p) = wt.path().to_str() {
            if std::path::Path::new(p).exists() {
                if let Err(e) = std::fs::remove_dir_all(p) {
                    // 目录删不掉(可能被占用)时先 lock 再 prune
                    let _ = wt.lock(Some("removing"));
                }
            }
        }
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).working_tree(true);
        wt.prune(Some(&mut prune)).context("prune worktree")?;
        let _ = self
            .repo
            .find_reference(&format!("refs/heads/{}", name))
            .and_then(|mut r| r.delete());
        Ok(())
    }
}

impl GitStatus {
    pub fn is_staged(&self) -> bool {
        matches!(self, GitStatus::Staged { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo() -> (tempfile::TempDir, Git) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        drop(cfg);
        let git = Git { repo };
        (tmp, git)
    }

    #[test]
    fn status_stage_commit_log() {
        let (tmp, git) = init_repo();
        fs::write(tmp.path().join("a.txt"), "hello\n").unwrap();
        let st = git.status().unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].status, GitStatus::New);

        git.stage(&[PathBuf::from("a.txt")]).unwrap();
        let st = git.status().unwrap();
        assert!(st[0].status.is_staged());

        git.commit("init").unwrap();
        assert_eq!(git.status().unwrap().len(), 0);
        let log = git.log(5).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "init");

        fs::write(tmp.path().join("a.txt"), "hello world\n").unwrap();
        let diff = git.diff_file(Path::new("a.txt")).unwrap();
        assert!(diff.contains("+hello world"), "diff: {diff}");
        assert!(diff.contains("-hello"), "diff: {diff}");
    }

    #[test]
    fn branch_name() {
        let (tmp, git) = init_repo();
        std::fs::write(tmp.path().join("x"), "1").unwrap();
        git.stage(&[PathBuf::from("x")]).unwrap();
        git.commit("c").unwrap();
        // 分支名可能是 master 或 main,不断言具体值
        let b = git.branch().unwrap();
        assert!(b == "master" || b == "main", "branch = {b}");
    }

    #[test]
    fn log_page_skips_already_loaded_commits() {
        let (tmp, git) = init_repo();
        for index in 1..=3 {
            std::fs::write(tmp.path().join("x"), index.to_string()).unwrap();
            git.stage(&[PathBuf::from("x")]).unwrap();
            git.commit(&format!("c{index}")).unwrap();
        }
        let first = git.log_page(0, 2).unwrap();
        let second = git.log_page(2, 2).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|row| row.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["c3", "c2"]
        );
        assert_eq!(
            second
                .iter()
                .map(|row| row.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["c1"]
        );
    }

    #[test]
    fn worktree_lifecycle() {
        let (_tmp, git) = init_repo();
        std::fs::write(git.root().join("x"), "1").unwrap();
        git.stage(&[PathBuf::from("x")]).unwrap();
        git.commit("c").unwrap();

        let path = git.worktree_create("demo-wt").unwrap();
        assert!(path.join("x").exists(), "worktree 应包含仓库文件");
        let list = git.worktree_list().unwrap();
        assert!(list.iter().any(|(n, _)| n == "demo-wt"), "list: {list:?}");

        git.worktree_remove("demo-wt").unwrap();
        let list = git.worktree_list().unwrap();
        assert!(
            !list.iter().any(|(n, _)| n == "demo-wt"),
            "remove 后不应残留: {list:?}"
        );
    }

    #[test]
    fn worktree_created_at_baseline_commit_carries_baseline_content() {
        let (tmp, git) = init_repo();
        std::fs::write(tmp.path().join("a.txt"), "v1\n").unwrap();
        git.stage(&[PathBuf::from("a.txt")]).unwrap();
        let first = {
            // 直接用 git2 拿第一个提交 oid(Git::commit 只能追加到 HEAD)
            let repo = git2::Repository::open(tmp.path()).unwrap();
            let sig = repo.signature().unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("a.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "v1", &tree, &[])
                .unwrap()
        };
        std::fs::write(tmp.path().join("a.txt"), "v2\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "only-in-v2\n").unwrap();
        git.stage(&[PathBuf::from("a.txt"), PathBuf::from("b.txt")])
            .unwrap();
        git.commit("v2").unwrap();

        // 基线 = 第一个提交:worktree 只包含 v1 内容
        let wt = git.worktree_create_at("baseline-wt", Some(first)).unwrap();
        let norm = |p: PathBuf| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
        assert_eq!(norm(wt.join("a.txt")), "v1\n");
        assert!(!wt.join("b.txt").exists(), "基线外文件不得出现");
        git.worktree_remove("baseline-wt").unwrap();
    }

    #[test]
    fn integration_ref_advance_read_and_cleanup() {
        let (tmp, git) = init_repo();
        std::fs::write(tmp.path().join("f"), "1").unwrap();
        git.stage(&[PathBuf::from("f")]).unwrap();
        git.commit("base").unwrap();
        let repo = git2::Repository::open(tmp.path()).unwrap();
        let head_tree = repo.head().unwrap().peel_to_commit().unwrap().tree_id();

        let refname = Git::integration_ref(7, 3);
        assert!(git.read_ref(&refname).unwrap().is_none());
        let c1 = git
            .advance_integration_ref(&refname, head_tree, "int-1")
            .unwrap();
        assert_eq!(git.read_ref(&refname).unwrap(), Some(c1));
        // 第二次推进:父提交是 ref 当前目标(线性集成历史)
        let c2 = git
            .advance_integration_ref(&refname, head_tree, "int-2")
            .unwrap();
        assert_eq!(git.read_ref(&refname).unwrap(), Some(c2));
        let commit = repo.find_commit(c2).unwrap();
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.parent_id(0).unwrap(), c1);

        // 同任务另一 revision 的 ref 不被误删
        let other = Git::integration_ref(7, 9);
        git.advance_integration_ref(&other, head_tree, "keep")
            .unwrap();
        assert_eq!(git.delete_integration_refs(7).unwrap(), 2);
        assert!(git.read_ref(&refname).unwrap().is_none());
        assert!(git.read_ref(&other).unwrap().is_none());
        // 不存在于 refs/heads:普通分支列表看不到
        let branch = git.branch().unwrap();
        assert!(!branch.contains("mf/integration"));
    }
}
