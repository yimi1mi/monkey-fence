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
    scanning: bool,
    queue: Arc<Mutex<FileQueue>>,
    stop: Arc<AtomicBool>,
}

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
                    .threads(4)
                    .build_parallel();
                let mut batch = Vec::new();
                walker.run(|| {
                    let queue = queue.clone();
                    let mut batch = Vec::new();
                    Box::new(move |entry| {
                        match entry {
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
                        }
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
                if watcher.watch(&root, notify::RecursiveMode::Recursive).is_err() {
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
            scanning: true,
            queue,
            stop,
        };
        idx.start_drain(cx);
        idx
    }

    fn start_drain(&self, cx: &mut Context<Self>) {
        let queue = self.queue.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let (adds, removes) = {
                    let mut q = queue.lock();
                    (std::mem::take(&mut q.adds), std::mem::take(&mut q.removes))
                };
                if adds.is_empty() && removes.is_empty() {
                    continue;
                }
                let n = this.update(cx, |idx, cx| {
                    for p in adds {
                        idx.files.insert(p);
                    }
                    for p in removes {
                        idx.files.remove(&p);
                    }
                    cx.notify();
                    idx.files.len()
                });
                if n.is_err() {
                    break;
                }
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

    /// 相对 root 的路径列表(快速打开用)
    pub fn relative_paths(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|p| {
                p.strip_prefix(&self.root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }
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
