mod editor;
mod theme;

use gpui::{div, prelude::*, App, Bounds, Context, WindowBounds, WindowOptions, size, px};

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| EditorSmoke::new(cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

/// 冒烟视图:加载自身 main.rs 的编辑器
pub struct EditorSmoke {
    pub editor: gpui::Entity<editor::Editor>,
}

impl EditorSmoke {
    fn new(cx: &mut Context<Self>) -> Self {
        let buffer = cx.new(|_| {
            let path = std::path::PathBuf::from("crates/mf/src/main.rs");
            mf_core::buffer::Buffer::load(&path)
                .unwrap_or_else(|_| mf_core::buffer::Buffer::empty(Some(path)))
        });
        let editor = cx.new(|cx| editor::Editor::new(buffer, cx));
        Self { editor }
    }
}

impl Render for EditorSmoke {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let ed = self.editor.clone();
        div().size_full().child(ed)
    }
}
