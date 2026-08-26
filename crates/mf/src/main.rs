mod editor;
mod file_index;
mod file_tree;
mod quick_open;
mod theme;
mod workspace;

use gpui::prelude::*;
use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use workspace::Workspace;

fn main() {
    // CLI:monkeyfence [项目路径]
    let project = std::env::args().nth(1).map(std::path::PathBuf::from);

    gpui_platform::application().run(move |cx: &mut App| {
        bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(800.0)), cx);
        let project = project.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("MonkeyFence".into()),
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|cx| {
                    let mut ws = Workspace::new(cx);
                    if let Some(p) = &project {
                        if p.is_dir() {
                            ws.open_folder(p.clone(), cx);
                        } else if p.is_file() {
                            if let Some(parent) = p.parent() {
                                ws.open_folder(parent.to_path_buf(), cx);
                            }
                            ws.open_path(p, cx);
                        }
                    }
                    ws
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

macro_rules! bind_many {
    ($cx:expr, $ctx:expr; $(($key:expr, $action:expr)),+ $(,)?) => {
        $( $cx.bind_keys([KeyBinding::new($key, $action, Some($ctx))]); )+
    };
}

fn bind_keys(cx: &mut App) {
    use editor as ed;
    use quick_open as qo;
    use workspace as ws;

    // 编辑器
    bind_many!(cx, "Editor";
        ("backspace", ed::Backspace),
        ("delete", ed::Delete),
        ("left", ed::Left),
        ("right", ed::Right),
        ("up", ed::Up),
        ("down", ed::Down),
        ("shift-left", ed::SelectLeft),
        ("shift-right", ed::SelectRight),
        ("shift-up", ed::SelectUp),
        ("shift-down", ed::SelectDown),
        ("home", ed::Home),
        ("end", ed::End),
        ("shift-home", ed::SelectHome),
        ("shift-end", ed::SelectEnd),
        ("pageup", ed::PageUp),
        ("pagedown", ed::PageDown),
        ("ctrl-left", ed::WordLeft),
        ("ctrl-right", ed::WordRight),
        ("ctrl-backspace", ed::DeleteWordBackward),
        ("ctrl-delete", ed::DeleteWordForward),
        ("ctrl-a", ed::SelectAll),
        ("ctrl-z", ed::Undo),
        ("ctrl-y", ed::Redo),
        ("ctrl-shift-z", ed::Redo),
        ("ctrl-s", ed::Save),
        ("enter", ed::Newline),
        ("tab", ed::Tab),
        ("shift-tab", ed::Backtab),
        ("ctrl-d", ed::DuplicateLine),
        ("alt-up", ed::MoveLineUp),
        ("alt-down", ed::MoveLineDown),
    );

    // 工作区
    bind_many!(cx, "Workspace";
        ("ctrl-shift-o", ws::OpenFolder),
        ("ctrl-p", ws::QuickOpenFiles),
        ("ctrl-shift-p", ws::CommandPalette),
        ("ctrl-w", ws::CloseTab),
        ("ctrl-tab", ws::NextTab),
        ("ctrl-shift-tab", ws::PrevTab),
        ("ctrl-b", ws::ToggleLeftPanel),
        ("ctrl-shift-e", ws::ShowExplorer),
        ("ctrl-shift-g", ws::ShowVcs),
    );

    // 快速打开浮层
    bind_many!(cx, "QuickOpen";
        ("enter", qo::ConfirmItem),
        ("escape", qo::Dismiss),
        ("up", qo::SelectPrev),
        ("down", qo::SelectNext),
    );
}
