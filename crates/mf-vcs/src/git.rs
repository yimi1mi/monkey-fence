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
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;
        let mut out = Vec::new();
        for oid in revwalk.take(max) {
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
        let parent = self
            .root()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("仓库根没有父目录"))?
            .to_path_buf();
        let dir = parent.join(".worktrees").join(name);
        anyhow::ensure!(!dir.exists(), "worktree 目录已存在: {}", dir.display());
        std::fs::create_dir_all(dir.parent().unwrap_or(&parent))?;
        let head = self.repo.head()?.peel_to_commit()?;
        let branch_ref = format!("refs/heads/{}", name);
        let mut opts = git2::WorktreeAddOptions::new();
        let reference = self
            .repo
            .reference(&branch_ref, head.id(), false, "mf worktree create")
            .with_context(|| format!("创建分支失败: {}", branch_ref))?;
        opts.reference(Some(&reference));
        let wt = self.repo.worktree(name, &dir, Some(&opts))?;
        let _ = wt;
        Ok(dir)
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
}
