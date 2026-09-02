//! legacy UI 的 gpui 颜色换算;终端模拟核心已迁至
//! `mf-terminal/src/term_screen.rs`(T3a),此处 re-export 保持旧路径兼容。

pub use mf_terminal::term_screen::*;

/// 颜色 → gpui Hsla
pub fn color_to_hsla(c: Color, default: [u8; 3]) -> gpui::Hsla {
    let rgb = if c.default { default } else { c.rgb };
    gpui::Rgba {
        r: rgb[0] as f32 / 255.,
        g: rgb[1] as f32 / 255.,
        b: rgb[2] as f32 / 255.,
        a: 1.0,
    }
    .into()
}
