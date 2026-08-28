use gpui::prelude::*;
use gpui::{App, Context, Entity};
use notify::Watcher;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 项目文件索引:后台并行扫描(gitignore 感知)+ 文件监听,流式更新
pub struct FileIndex {
    root: PathBuf,
    files: BTreeSet<PathBuf>,
    /// 相对路径缓存:drain 时增量维护,避免每次查询全量分配。
    rel_cache: Arc<Vec<String>>,
    scanning: bool,
    queue: Arc<Mutex<FileQueue>>,
    stop: Arc<AtomicBool>,
}

/// 快速打开不需要的生成物目录(P4/非 git 工程没有 .gitignore 兜底,
/// Unity 的 Library/Temp 会有几十万文件)。
fn is_junk_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "library" | "temp" | "logs" | "obj" | "bin" | "node_modules" | "target"
    )
}

/// 单个 drain 周期在主线程插入的上限:防止首批几十万条一次性卡死 UI。
const DRAIN_BATCH_MAX: usize = 4000;

#[derive(Default)]
struct FileQueue {
    adds: Vec<PathBuf>,
    removes: Vec<PathBuf>,
}

impl FileIndex {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let queue = Arc::new(Mutex::new(FileQueue::default()));
        let stop = Arc::new(AtomicBool::new(false));

        // 扫描线程
        {
            let queue = queue.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                let walker = ignore::WalkBuilder::new(&root)
                    .hidden(true)
                    .git_ignore(true)
                    .git_global(true)
                    .git_exclude(true)
                    .parents(true)
                    .filter_entry(|entry| {
                        entry
                            .file_type()
                            .map(|t| {
                                !t.is_dir() || !is_junk_dir(&entry.file_name().to_string_lossy())
                            })
                            .unwrap_or(true)
                    })
                    .threads(4)
                    .build_parallel();
                let mut batch = Vec::new();
                walker.run(|| {
                    let queue = queue.clone();
                    let mut batch = Vec::new();
                    Box::new(move |entry| match entry {
                        Ok(e) => {
                            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                                batch.push(e.into_path());
                            }
                            if batch.len() >= 500 {
                                let mut q = queue.lock();
                                q.adds.append(&mut batch);
                            }
                            ignore::WalkState::Continue
                        }
                        Err(_) => ignore::WalkState::Continue,
                    })
                });
                let _ = &mut batch;
                queue.lock().adds.extend(batch);
            });
        }

        // 文件监听
        {
            let queue = queue.clone();
            let root = root.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                let mut watcher = match notify::recommended_watcher(tx) {
                    Ok(w) => w,
                    Err(_) => return,
                };
                if watcher
                    .watch(&root, notify::RecursiveMode::Recursive)
                    .is_err()
                {
                    return;
                }
                while !stop.load(Ordering::Relaxed) {
                    // 批量收集当前积压事件,配合 sleep 去抖
                    let mut batch: Vec<notify::Event> = Vec::new();
                    loop {
                        match rx.try_recv() {
                            Ok(Ok(ev)) => batch.push(ev),
                            Ok(Err(_)) => break,
                            Err(_) => break,
                        }
                    }
                    if !batch.is_empty() {
                        let mut q = queue.lock();
                        for ev in batch {
                            match ev.kind {
                                notify::EventKind::Create(_) => {
                                    if let Some(p) = ev.paths.first() {
                                        if p.is_file() {
                                            q.adds.push(p.clone());
                                        }
                                    }
                                }
                                notify::EventKind::Remove(_) => {
                                    if let Some(p) = ev.paths.first() {
                                        q.removes.push(p.clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            });
        }

        let idx = Self {
            root,
            files: BTreeSet::new(),
            rel_cache: Arc::new(Vec::new()),
            scanning: true,
            queue,
            stop,
        };
        idx.start_drain(cx);
        idx
    }

    fn start_drain(&self, cx: &mut Context<Self>) {
        let queue = self.queue.clone();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            // 有界取批:超出的留给下一轮,主线程每轮最多插入 DRAIN_BATCH_MAX 条
            let take_batch = |v: &mut Vec<PathBuf>| -> Vec<PathBuf> {
                if v.len() > DRAIN_BATCH_MAX {
                    let tail = v.split_off(DRAIN_BATCH_MAX);
                    std::mem::replace(v, tail)
                } else {
                    std::mem::take(v)
                }
            };
            let (adds, removes) = {
                let mut q = queue.lock();
                (take_batch(&mut q.adds), take_batch(&mut q.removes))
            };
            if adds.is_empty() && removes.is_empty() {
                continue;
            }
            let n = this.update(cx, |idx, cx| {
                idx.apply_updates(adds, &removes);
                cx.notify();
                idx.files.len()
            });
            if n.is_err() {
                break;
            }
        })
        .detach();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 应用一批增量:纯新增走增量缓存;有删除则全量重建(低频)。
    fn apply_updates(&mut self, adds: Vec<PathBuf>, removes: &[PathBuf]) {
        if !removes.is_empty() {
            for p in removes {
                self.files.remove(p);
            }
            for p in &adds {
                self.files.insert(p.clone());
            }
            let root = &self.root;
            self.rel_cache = Arc::new(
                self.files
                    .iter()
                    .map(|p| rel_string(root, p))
                    .collect::<Vec<_>>(),
            );
        } else {
            let root = &self.root;
            let cache = Arc::make_mut(&mut self.rel_cache);
            for p in adds {
                if self.files.insert(p.clone()) {
                    cache.push(rel_string(root, &p));
                }
            }
        }
    }

    /// 相对路径快照(Arc 共享,后台模糊匹配直接用它,不再每次全量分配)。
    pub fn relative_paths_arc(&self) -> Arc<Vec<String>> {
        self.rel_cache.clone()
    }
}

fn rel_string(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 在实体上创建索引的便捷入口
pub fn entity_for(root: PathBuf, cx: &mut App) -> Entity<FileIndex> {
    cx.new(|cx| FileIndex::new(root, cx))
}
