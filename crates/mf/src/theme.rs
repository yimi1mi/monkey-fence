use gpui::Hsla;

/// MonkeyFence 深色主题(自有配色,Catppuccin 风格基调配色重调)
pub struct Theme;

impl Theme {
    pub const BG: u32 = 0x16161e;
    pub const BG_PANEL: u32 = 0x1c1c26;
    pub const BG_ELEVATED: u32 = 0x24242f;
    pub const BG_HOVER: u32 = 0x2c2c3a;
    pub const BG_ACTIVE: u32 = 0x333346;
    pub const BORDER: u32 = 0x30303e;
    pub const FG: u32 = 0xd6d9e3;
    pub const FG_DIM: u32 = 0x8a8ca0;
    pub const FG_FAINT: u32 = 0x5c5e70;
    pub const ACCENT: u32 = 0x7aa2f7;
    pub const ACCENT_DIM: u32 = 0x3d5a9e;
    pub const SUCCESS: u32 = 0x9ece6a;
    pub const WARNING: u32 = 0xe0af68;
    pub const DANGER: u32 = 0xf7768e;
    pub const GUTTER_FG: u32 = 0x464860;
    pub const SELECTION: Hsla = Hsla {
        h: 220. / 360.,
        s: 0.65,
        l: 0.4,
        a: 0.35,
    };
    pub const CURSOR: u32 = 0x7aa2f7;

    /// 语法高亮标签 → 颜色
    pub fn syntax(tag: mf_core::highlight::HighlightTag) -> u32 {
        use mf_core::highlight::HighlightTag::*;
        match tag {
            Comment => 0x565f89,
            String => 0x9ece6a,
            Keyword => 0xbb9af7,
            Number => 0xff9e64,
            Function => 0x7aa2f7,
            Type => 0x2ac3de,
            Variable => 0xc0caf5,
            Constant => 0xff9e64,
            Property => 0x73daca,
            Operator => 0x89ddff,
            Punctuation => 0x8992b4,
            Tag => 0xf7768e,
            Attribute => 0xe0af68,
        }
    }
}
