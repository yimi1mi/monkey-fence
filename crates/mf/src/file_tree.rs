use gpui::prelude::*;
use gpui::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// 文件树:懒加载目录 + uniform_list 虚拟滚动
pub struct FileTree {
    root: PathBuf,
    expanded: HashSet<PathBuf>,
    children: Rc<RefCell<HashMap<PathBuf, Rc<Vec<TreeEntry>>>>>,
    rows: Vec<Row>,
    selected: Option<PathBuf>,
    on_open: Option<Box<dyn Fn(&Path, &mut Window, &mut App)>>,
    scroll_handle: UniformListScrollHandle,
}

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
struct Row {
    entry: TreeEntry,
    depth: usize,
}

fn read_children(dir: &Path) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(TreeEntry {
                path: e.path(),
                name,
                is_dir,
            });
        }
    }
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    out
}

impl FileTree {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut tree = Self {
            root: root.clone(),
            expanded: HashSet::new(),
            children: Rc::new(RefCell::new(HashMap::new())),
            rows: Vec::new(),
            selected: None,
            on_open: None,
            scroll_handle: UniformListScrollHandle::new(),
        };
        tree.expanded.insert(root);
        tree.rebuild();
        let _ = cx;
        tree
    }

    pub fn set_on_open(&mut self, cb: impl Fn(&Path, &mut Window, &mut App) + 'static) {
        self.on_open = Some(Box::new(cb));
    }

    pub fn refresh_dir(&mut self, dir: &Path) {
        self.children.borrow_mut().remove(dir);
        self.rebuild();
    }

    pub fn refresh_all(&mut self) {
        self.children.borrow_mut().clear();
        self.rebuild();
    }

    fn children_of(&self, dir: &Path) -> Rc<Vec<TreeEntry>> {
        let mut cache = self.children.borrow_mut();
        cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| Rc::new(read_children(dir)))
            .clone()
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        let root_entry = TreeEntry {
            path: self.root.clone(),
            name: self
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.root.to_string_lossy().into_owned()),
            is_dir: true,
        };
        rows.push(Row {
            entry: root_entry,
            depth: 0,
        });
        self.walk(&self.root.clone(), 1, &mut rows);
        self.rows = rows;
    }

    fn walk(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        if !self.expanded.contains(dir) {
            return;
        }
        for entry in self.children_of(dir).iter() {
            rows.push(Row {
                entry: entry.clone(),
                depth,
            });
            if entry.is_dir {
                self.walk(&entry.path, depth + 1, rows);
            }
        }
    }

    fn toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.expanded.contains(path) {
            self.expanded.remove(path);
        } else {
            self.expanded.insert(path.to_path_buf());
        }
        self.rebuild();
        cx.notify();
    }

    fn click_row(&mut self, path: &Path, is_dir: bool, window: &mut Window, cx: &mut App) {
        if is_dir {
            return;
        }
        self.selected = Some(path.to_path_buf());
        if let Some(cb) = &self.on_open {
            cb(path, window, cx);
        }
    }
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<(TreeEntry, usize)> = self
            .rows
            .iter()
            .map(|r| (r.entry.clone(), r.depth))
            .collect();
        let selected = self.selected.clone();
        div().id("file-tree").size_full().flex().flex_col().child(
            uniform_list(
                "file-tree-list",
                rows.len(),
                cx.processor(move |this, range, _window, cx| {
                    let mut out = Vec::new();
                    for ix in range {
                        let Some((entry, depth)) = rows
                            .get(ix)
                            .map(|(e, d): &(TreeEntry, usize)| (e.clone(), *d))
                        else {
                            continue;
                        };
                        let is_selected =
                            selected.as_ref().map(|s| *s == entry.path).unwrap_or(false);
                        let name_color = if entry.is_dir {
                            crate::theme::Theme::fg()
                        } else {
                            crate::theme::Theme::fg_dim()
                        };
                        let arrow = if entry.is_dir {
                            if this.expanded.contains(&entry.path) {
                                "▾"
                            } else {
                                "▸"
                            }
                        } else {
                            " "
                        };
                        out.push(
                            div()
                                .id(ElementId::Name(
                                    format!("tree-{}", entry.path.display()).into(),
                                ))
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .pl(px((depth as f32 * 12.0 + 8.0) as f32))
                                .pr_2()
                                .when(is_selected, |d| d.bg(rgb(crate::theme::Theme::bg_active())))
                                .hover(|d| d.bg(rgb(crate::theme::Theme::bg_hover())))
                                .cursor_pointer()
                                .text_size(px(13.))
                                .child(
                                    div()
                                        .w(px(14.))
                                        .text_color(rgb(crate::theme::Theme::fg_faint()))
                                        .child(arrow),
                                )
                                .child(div().text_color(rgb(name_color)).child(entry.name.clone()))
                                .on_click({
                                    let entry = entry.clone();
                                    cx.listener(move |this: &mut FileTree, _, window, cx| {
                                        if entry.is_dir {
                                            this.toggle(&entry.path, cx);
                                        } else {
                                            this.click_row(&entry.path, false, window, cx);
                                        }
                                    })
                                }),
                        );
                    }
                    out
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1(),
        )
    }
}
