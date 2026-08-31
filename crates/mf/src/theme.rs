use gpui::Hsla;
use std::sync::RwLock;

/// 双色板;运行时可切(设置页外观),渲染处每帧经函数访问器读取
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: u32,
    pub bg_panel: u32,
    pub bg_elevated: u32,
    pub bg_hover: u32,
    pub bg_active: u32,
    pub border: u32,
    pub fg: u32,
    pub fg_dim: u32,
    pub fg_faint: u32,
    pub accent: u32,
    pub accent_dim: u32,
    pub success: u32,
    pub warning: u32,
    pub danger: u32,
    pub gutter_fg: u32,
    pub cursor: u32,
    // 语法色板
    pub syn_comment: u32,
    pub syn_string: u32,
    pub syn_keyword: u32,
    pub syn_number: u32,
    pub syn_function: u32,
    pub syn_type: u32,
    pub syn_variable: u32,
    pub syn_constant: u32,
    pub syn_property: u32,
    pub syn_operator: u32,
    pub syn_punctuation: u32,
    pub syn_tag: u32,
    pub syn_attribute: u32,
}

pub const NIGHT_VOYAGE_ID: &str = "night-voyage";
pub const MORNING_MIST_ID: &str = "morning-mist";

/// 夜航:低眩光海军蓝暗色主题。
pub const DARK: Palette = Palette {
    bg: 0x151b24,
    bg_panel: 0x1c2430,
    bg_elevated: 0x232d3a,
    bg_hover: 0x2b3746,
    bg_active: 0x273546,
    border: 0x334152,
    fg: 0xdde5ef,
    fg_dim: 0x96a4b5,
    fg_faint: 0x607084,
    accent: 0x62aaf7,
    accent_dim: 0x315f91,
    success: 0x4ec97e,
    warning: 0xe0b23c,
    danger: 0xe05c5c,
    gutter_fg: 0x4a5361,
    cursor: 0x4d9fff,
    syn_comment: 0x5a9970,
    syn_string: 0xce9178,
    syn_keyword: 0xc586c0,
    syn_number: 0xb5cea8,
    syn_function: 0xdcdcaa,
    syn_type: 0x4ec9b0,
    syn_variable: 0x9cdcfe,
    syn_constant: 0x4fc1ff,
    syn_property: 0x9cdcfe,
    syn_operator: 0xd4d4d4,
    syn_punctuation: 0xadb5c0,
    syn_tag: 0x569cd6,
    syn_attribute: 0x9cdcfe,
};

/// 晨雾:略带暖灰的纸面亮色主题。
pub const LIGHT: Palette = Palette {
    bg: 0xf3f2ee,
    bg_panel: 0xfbfaf7,
    bg_elevated: 0xeae9e4,
    bg_hover: 0xe1e4e8,
    bg_active: 0xd8e0eb,
    border: 0xd4d3ce,
    fg: 0x252a31,
    fg_dim: 0x5f6874,
    fg_faint: 0x8c949d,
    accent: 0x356fc4,
    accent_dim: 0xa9bfdd,
    success: 0x3f8f4f,
    warning: 0xb07d24,
    danger: 0xc93a52,
    gutter_fg: 0xb6bcc6,
    cursor: 0x2f6fdb,
    syn_comment: 0x8a919c,
    syn_string: 0x3f8f4f,
    syn_keyword: 0x8a4fbf,
    syn_number: 0xb35c1e,
    syn_function: 0x2f6fdb,
    syn_type: 0x0f7f8f,
    syn_variable: 0x343a44,
    syn_constant: 0xb35c1e,
    syn_property: 0x1c7f6b,
    syn_operator: 0x4f8fbf,
    syn_punctuation: 0x6a7280,
    syn_tag: 0xc93a52,
    syn_attribute: 0xb07d24,
};

static PALETTE: RwLock<Palette> = RwLock::new(DARK);
static THEME_ID: RwLock<&'static str> = RwLock::new(NIGHT_VOYAGE_ID);

/// 按稳定 ID 切换主题；未知 ID 安全回退到夜航。
pub fn set_theme_id(id: &str) -> &'static str {
    let (id, palette) = match id {
        MORNING_MIST_ID => (MORNING_MIST_ID, LIGHT),
        _ => (NIGHT_VOYAGE_ID, DARK),
    };
    *PALETTE.write().unwrap() = palette;
    *THEME_ID.write().unwrap() = id;
    id
}

pub fn current_theme_id() -> &'static str {
    *THEME_ID.read().unwrap()
}

pub fn is_light() -> bool {
    current_theme_id() == MORNING_MIST_ID
}

fn p() -> Palette {
    *PALETTE.read().unwrap()
}

/// 兼容旧调用面的函数化访问器:Theme::X 常量 → Theme::x() 函数
pub struct Theme;

#[allow(non_snake_case)]
impl Theme {
    pub fn bg() -> u32 {
        p().bg
    }
    pub fn bg_panel() -> u32 {
        p().bg_panel
    }
    pub fn bg_elevated() -> u32 {
        p().bg_elevated
    }
    pub fn bg_hover() -> u32 {
        p().bg_hover
    }
    pub fn bg_active() -> u32 {
        p().bg_active
    }
    pub fn border() -> u32 {
        p().border
    }
    pub fn fg() -> u32 {
        p().fg
    }
    pub fn fg_dim() -> u32 {
        p().fg_dim
    }
    pub fn fg_faint() -> u32 {
        p().fg_faint
    }
    pub fn accent() -> u32 {
        p().accent
    }
    pub fn accent_dim() -> u32 {
        p().accent_dim
    }
    pub fn success() -> u32 {
        p().success
    }
    pub fn warning() -> u32 {
        p().warning
    }
    pub fn danger() -> u32 {
        p().danger
    }
    pub fn gutter_fg() -> u32 {
        p().gutter_fg
    }
    pub fn cursor() -> u32 {
        p().cursor
    }

    pub fn selection() -> Hsla {
        Hsla {
            h: 220. / 360.,
            s: 0.65,
            l: if is_light() { 0.75 } else { 0.4 },
            a: 0.35,
        }
    }

    /// 语法高亮标签 → 颜色(随主题切换)
    pub fn syntax(tag: mf_core::highlight::HighlightTag) -> u32 {
        use mf_core::highlight::HighlightTag::*;
        let pal = p();
        match tag {
            Comment => pal.syn_comment,
            String => pal.syn_string,
            Keyword => pal.syn_keyword,
            Number => pal.syn_number,
            Function => pal.syn_function,
            Type => pal.syn_type,
            Variable => pal.syn_variable,
            Constant => pal.syn_constant,
            Property => pal.syn_property,
            Operator => pal.syn_operator,
            Punctuation => pal.syn_punctuation,
            Tag => pal.syn_tag,
            Attribute => pal.syn_attribute,
        }
    }
}
