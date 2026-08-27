//! VT/ANSI 终端模拟核心:字节流 → 网格(char + 颜色/属性),支持 TUI 应用。
//!
//! 覆盖:UTF-8 解码(跨包)、SGR 颜色(基本 16 色/256 色/真彩/加粗/下划线/反显)、
//! 光标移动(CUP/CUU-CUD-CUF-CUB/CH)、清屏清行(ED/EL/ECH)、
//! 交替屏幕(?1049 等)、滚动区(DECSTBM/SU/SD)、行插入删除(ICH/DCH/DL/IL)、
//! 光标显隐(?25)、DECSC/DECRC、OSC 0/2 窗格标题、 BEL/制表/退格。

/// 终端颜色(None = 默认前景/背景)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub rgb: [u8; 3],
    pub default: bool,
}

impl Color {
    pub const DEF: Color = Color { rgb: [0, 0, 0], default: true };
    pub fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { rgb: [r, g, b], default: false }
    }
}

/// 单元格:字符 + 样式
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Cell {
    pub const BLANK: Cell = Cell {
        ch: ' ',
        fg: Color::DEF,
        bg: Color::DEF,
        bold: false,
        underline: false,
        reverse: false,
    };
}

/// 16 色调色板(xterm 系)
pub fn palette(i: u8) -> Color {
    const T: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), (0xcd, 0x3a, 0x3a), (0x0e, 0xb0, 0x5c), (0xc9, 0x51, 0x00),
        (0x0f, 0x5f, 0xff), (0xab, 0x5a, 0xff), (0x0e, 0xb0, 0xb0), (0xe6, 0xe6, 0xe6),
        (0x4d, 0x4d, 0x4d), (0xff, 0x6e, 0x6e), (0x3c, 0xf7, 0x6a), (0xff, 0xa5, 0x4f),
        (0x69, 0xbe, 0xff), (0xd3, 0x8a, 0xff), (0x3c, 0xf7, 0xf7), (0xff, 0xff, 0xff),
    ];
    Color::rgb(T[i as usize % 16].0, T[i as usize % 16].1, T[i as usize % 16].2)
}

/// 256 色立方体/灰度
fn palette256(i: u8) -> Color {
    if i < 16 {
        palette(i)
    } else if i < 232 {
        let n = i - 16;
        let step = [0, 95, 135, 175, 215, 255];
        let r = step[(n / 36) as usize];
        let g = step[((n % 36) / 6) as usize];
        let b = step[(n % 6) as usize];
        Color::rgb(r, g, b)
    } else {
        let v = 8 + (i - 232) as u8 * 10;
        Color::rgb(v, v, v)
    }
}

#[derive(Clone, Copy)]
struct Pen {
    fg: Color,
    bg: Color,
    bold: bool,
    underline: bool,
    reverse: bool,
}

impl Default for Pen {
    fn default() -> Self {
        Pen { fg: Color::DEF, bg: Color::DEF, bold: false, underline: false, reverse: false }
    }
}

/// 终端屏幕:主/副(alt)两块网格 + 光标 + 滚动区
pub struct Screen {
    pub rows: usize,
    pub cols: usize,
    main: Vec<Vec<Cell>>,
    alt: Vec<Vec<Cell>>,
    alt_active: bool,
    cursor: (usize, usize), // (row, col)
    saved_cursor: (usize, usize),
    saved_pen: Pen,
    pen: Pen,
    scroll_top: usize,
    scroll_bottom: usize, // 含
    // 解析状态
    state: ParseState,
    utf8_buf: Vec<u8>,
    utf8_need: usize,
    interm: Vec<u8>,
    params: Vec<u32>,
    prefix: u8, // '<' '=' '>' '?' 等
    final_byte: u8,
    osc_buf: Vec<u8>,
    pub title: String,
    pub cursor_visible: bool,
    /// 视口是否需要重绘(供上层节流)
    pub dirty: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc, // OSC 中遇 ESC(可能为 ST)
    EscStr, // ESC P/APC/PM 等字符串序列(丢弃直到 ST)
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(2);
        let cols = cols.max(4);
        Self {
            rows,
            cols,
            main: vec![vec![Cell::BLANK; cols]; rows],
            alt: vec![vec![Cell::BLANK; cols]; rows],
            alt_active: false,
            cursor: (0, 0),
            saved_cursor: (0, 0),
            saved_pen: Pen::default(),
            pen: Pen::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            state: ParseState::Ground,
            utf8_buf: Vec::new(),
            utf8_need: 0,
            interm: Vec::new(),
            params: Vec::new(),
            prefix: 0,
            final_byte: 0,
            osc_buf: Vec::new(),
            title: String::new(),
            cursor_visible: true,
            dirty: true,
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(2);
        let cols = cols.max(4);
        self.rows = rows;
        self.cols = cols;
        for grid in [&mut self.main, &mut self.alt] {
            grid.resize(rows, vec![Cell::BLANK; cols]);
            for row in grid.iter_mut() {
                row.resize(cols, Cell::BLANK);
            }
        }
        self.cursor.0 = self.cursor.0.min(rows - 1);
        self.cursor.1 = self.cursor.1.min(cols - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.dirty = true;
    }

    fn grid(&self) -> &Vec<Vec<Cell>> {
        if self.alt_active { &self.alt } else { &self.main }
    }

    fn grid_mut(&mut self) -> &mut Vec<Vec<Cell>> {
        if self.alt_active { &mut self.alt } else { &mut self.main }
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        let mut c = self.grid().get(row).and_then(|r| r.get(col)).copied().unwrap_or(Cell::BLANK);
        if c.reverse {
            std::mem::swap(&mut c.fg, &mut c.bg);
        }
        c
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// 喂入字节流(UTF-8 + VT 序列,状态跨调用保持)
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
        self.dirty = true;
    }

    fn step(&mut self, b: u8) {
        match self.state {
            ParseState::Ground => self.ground(b),
            ParseState::Esc => match b {
                b'[' => {
                    self.state = ParseState::Csi;
                    self.params.clear();
                    self.interm.clear();
                    self.prefix = 0;
                }
                b']' => {
                    self.state = ParseState::Osc;
                    self.osc_buf.clear();
                }
                b'P' | b'X' | b'^' | b'_' => self.state = ParseState::EscStr,
                b'7' => {
                    self.saved_cursor = self.cursor;
                    self.saved_pen = self.pen;
                    self.state = ParseState::Ground;
                }
                b'8' => {
                    self.cursor = self.saved_cursor;
                    self.pen = self.saved_pen;
                    self.state = ParseState::Ground;
                }
                b'M' => {
                    self.reverse_index();
                    self.state = ParseState::Ground;
                }
                b'D' => {
                    self.linefeed();
                    self.state = ParseState::Ground;
                }
                b'E' => {
                    self.cursor.1 = 0;
                    self.linefeed();
                    self.state = ParseState::Ground;
                }
                b'c' => {
                    self.full_reset();
                    self.state = ParseState::Ground;
                }
                _ => self.state = ParseState::Ground,
            },
            ParseState::Csi => match b {
                b'0'..=b'9' => {
                    let last = self.params.last_mut();
                    match last {
                        Some(v) => *v = (*v).saturating_mul(10).saturating_add((b - b'0') as u32),
                        None => self.params.push((b - b'0') as u32),
                    }
                }
                b';' => self.params.push(0),
                b':' => self.params.push(0),
                b'?' | b'<' | b'=' | b'>' => self.prefix = b,
                0x20..=0x2f => self.interm.push(b),
                0x40..=0x7e => {
                    self.final_byte = b;
                    self.csi_dispatch();
                    self.state = ParseState::Ground;
                }
                _ => self.state = ParseState::Ground,
            },
            ParseState::Osc => {
                // OSC 以 BEL 或 ST(ESC \) 结束
                if b == 0x07 {
                    self.osc_done();
                } else if b == 0x1b {
                    self.state = ParseState::OscEsc;
                } else {
                    self.osc_buf.push(b);
                    if self.osc_buf.len() > 4096 {
                        self.osc_buf.clear();
                        self.state = ParseState::Ground;
                    }
                }
            }
            ParseState::OscEsc => {
                if b == b'\\' {
                    self.osc_done();
                } else {
                    self.state = ParseState::Ground;
                }
            }
            ParseState::EscStr => {
                if b == 0x1b {
                    self.state = ParseState::Esc; // 可能是 ESC \ 结束
                }
                // 其余丢弃
            }
        }
    }

    fn osc_done(&mut self) {
        self.state = ParseState::Ground;
        let s = String::from_utf8_lossy(&self.osc_buf).into_owned();
        // OSC 0/2:设置标题
        let payload = s.strip_prefix("0;").or_else(|| s.strip_prefix("2;"));
        if let Some(t) = payload {
            let t = t.trim();
            if !t.is_empty() {
                self.title = t.chars().take(120).collect();
            }
        }
    }

    fn ground(&mut self, b: u8) {
        match b {
            0x1b => self.state = ParseState::Esc,
            b'\r' => self.cursor.1 = 0,
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            0x08 => {
                if self.cursor.1 > 0 {
                    self.cursor.1 -= 1;
                }
            }
            b'\t' => {
                let next = ((self.cursor.1 / 8) + 1) * 8;
                self.cursor.1 = next.min(self.cols - 1);
            }
            0x07 => {} // BEL
            _ if b < 0x20 || b == 0x7f => {}
            _ => {
                if b < 0x80 {
                    self.put_char(b as char);
                } else {
                    // UTF-8 多字节累积
                    if self.utf8_need == 0 {
                        self.utf8_buf.clear();
                        if b & 0xE0 == 0xC0 {
                            self.utf8_need = 2;
                            self.utf8_buf.push(b);
                        } else if b & 0xF0 == 0xE0 {
                            self.utf8_need = 3;
                            self.utf8_buf.push(b);
                        } else if b & 0xF8 == 0xF0 {
                            self.utf8_need = 4;
                            self.utf8_buf.push(b);
                        } else {
                            self.put_char(char::REPLACEMENT_CHARACTER);
                        }
                    } else {
                        if b & 0xC0 == 0x80 {
                            self.utf8_buf.push(b);
                            if self.utf8_buf.len() == self.utf8_need {
                                let s = std::str::from_utf8(&self.utf8_buf);
                                let ch = s.ok().and_then(|s| s.chars().next())
                                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                                self.utf8_need = 0;
                                self.put_char(ch);
                            }
                        } else {
                            // 非法序列
                            self.utf8_need = 0;
                            self.put_char(char::REPLACEMENT_CHARACTER);
                        }
                    }
                }
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        let (r, c) = self.cursor;
        let pen = self.pen;
        let cols = self.cols;
        let cell = Cell { ch, fg: pen.fg, bg: pen.bg, bold: pen.bold, underline: pen.underline, reverse: pen.reverse };
        {
            let grid = self.grid_mut();
            if let Some(row) = grid.get_mut(r) {
                if c < cols {
                    row[c] = cell;
                }
            }
        }
        if c + 1 < cols {
            self.cursor.1 = c + 1;
        } else {
            self.cursor.1 = cols - 1;
            self.linefeed();
        }
    }

    fn linefeed(&mut self) {
        if self.cursor.0 == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.0 + 1 < self.rows {
            self.cursor.0 += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.0 == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (top, bot, cols) = (self.scroll_top, self.scroll_bottom, self.cols);
        let grid = self.grid_mut();
        for _ in 0..n {
            grid.remove(top);
            grid.insert(bot, vec![Cell::BLANK; cols]);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bot, cols) = (self.scroll_top, self.scroll_bottom, self.cols);
        let grid = self.grid_mut();
        for _ in 0..n {
            grid.remove(bot);
            grid.insert(top, vec![Cell::BLANK; cols]);
        }
    }

    fn csi_dispatch(&mut self) {
        let p = |i: usize| -> u32 { self.params.get(i).copied().unwrap_or(0) };
        let p1 = |i: usize| -> u32 { let v = self.params.get(i).copied().unwrap_or(0); if v == 0 { 1 } else { v } };
        match (self.prefix, self.final_byte) {
            (0, b'm') => self.sgr(),
            (0, b'A') => self.cursor.0 = self.cursor.0.saturating_sub(p1(0) as usize),
            (0, b'B') => self.cursor.0 = (self.cursor.0 + p1(0) as usize).min(self.rows - 1),
            (0, b'C') => self.cursor.1 = (self.cursor.1 + p1(0) as usize).min(self.cols - 1),
            (0, b'D') => self.cursor.1 = self.cursor.1.saturating_sub(p1(0) as usize),
            (0, b'E') => {
                self.cursor.0 = (self.cursor.0 + p1(0) as usize).min(self.rows - 1);
                self.cursor.1 = 0;
            }
            (0, b'F') => {
                self.cursor.0 = self.cursor.0.saturating_sub(p1(0) as usize);
                self.cursor.1 = 0;
            }
            (0, b'G' | b'`') => self.cursor.1 = (p1(0) as usize - 1).min(self.cols - 1),
            (0, b'H' | b'f') => {
                self.cursor.0 = (p1(0) as usize - 1).min(self.rows - 1);
                self.cursor.1 = (p1(1) as usize - 1).min(self.cols - 1);
            }
            (0, b'd') => self.cursor.0 = (p1(0) as usize - 1).min(self.rows - 1),
            (0, b'J') => self.erase_display(p(0)),
            (0, b'K') => self.erase_line(p(0)),
            (0, b'X') => {
                // ECH:光标起擦 n 格
                let n = p1(0) as usize;
                let (r, c) = self.cursor;
                let cols = self.cols;
                let grid = self.grid_mut();
                if let Some(row) = grid.get_mut(r) {
                    for i in 0..n {
                        if c + i < cols {
                            row[c + i] = Cell::BLANK;
                        }
                    }
                }
            }
            (0, b'@') => {
                // ICH:插入空格
                let n = p1(0) as usize;
                let (r, c) = self.cursor;
                let cols = self.cols;
                let grid = self.grid_mut();
                if let Some(row) = grid.get_mut(r) {
                    for i in (c..cols).rev() {
                        if i + n < cols {
                            row[i + n] = row[i];
                        }
                    }
                    for cell in row.iter_mut().skip(c).take(n.min(cols - c)) {
                        *cell = Cell::BLANK;
                    }
                }
            }
            (0, b'P') => {
                // DCH:删除字符
                let n = p1(0) as usize;
                let (r, c) = self.cursor;
                let cols = self.cols;
                let grid = self.grid_mut();
                if let Some(row) = grid.get_mut(r) {
                    for i in c..cols {
                        row[i] = if i + n < cols { row[i + n] } else { Cell::BLANK };
                    }
                }
            }
            (0, b'L') => {
                // IL:插入行
                let n = p1(0) as usize;
                let (r, _) = self.cursor;
                let top = self.scroll_top;
                let bot = self.scroll_bottom;
                let cols = self.cols;
                let grid = self.grid_mut();
                for _ in 0..n {
                    if r <= bot {
                        grid.remove(bot.min(grid.len() - 1));
                        grid.insert(r.max(top), vec![Cell::BLANK; cols]);
                    }
                }
            }
            (0, b'M') => {
                // DL:删除行
                let n = p1(0) as usize;
                let (r, _) = self.cursor;
                let bot = self.scroll_bottom;
                let cols = self.cols;
                let grid = self.grid_mut();
                for _ in 0..n {
                    if r <= bot {
                        grid.remove(r);
                        grid.insert(bot.min(grid.len() - 1), vec![Cell::BLANK; cols]);
                    }
                }
            }
            (0, b'S') => self.scroll_up(p1(0) as usize),
            (0, b'T') => self.scroll_down(p1(0) as usize),
            (0, b'r') => {
                // DECSTBM 滚动区,并归位光标
                let top = p1(0) as usize - 1;
                let bot = (p1(1) as usize).max(1) as usize - 1;
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.rows - 1;
                }
                self.cursor = (0, 0);
            }
            (b'?', b'h') => self.decset(true, p(0)),
            (b'?', b'l') => self.decset(false, p(0)),
            _ => {} // 未知序列忽略
        }
    }

    fn decset(&mut self, on: bool, code: u32) {
        match code {
            25 => self.cursor_visible = on,
            47 | 1047 => self.set_alt(on, false),
            1048 => {
                if on {
                    self.saved_cursor = self.cursor;
                } else {
                    self.cursor = self.saved_cursor;
                }
            }
            1049 => self.set_alt(on, true),
            _ => {}
        }
    }

    fn set_alt(&mut self, on: bool, clear: bool) {
        if on && !self.alt_active {
            self.saved_cursor = self.cursor;
            self.alt_active = true;
            if clear {
                for row in self.alt.iter_mut() {
                    for cell in row.iter_mut() {
                        *cell = Cell::BLANK;
                    }
                }
            }
            self.cursor = (0, 0);
        } else if !on && self.alt_active {
            self.alt_active = false;
            self.cursor = self.saved_cursor;
        }
    }

    fn erase_display(&mut self, mode: u32) {
        let (cr, cc) = self.cursor;
        let (rows, cols) = (self.rows, self.cols);
        let grid = self.grid_mut();
        match mode {
            0 => {
                // 光标到末尾
                if let Some(row) = grid.get_mut(cr) {
                    for cell in row.iter_mut().skip(cc) {
                        *cell = Cell::BLANK;
                    }
                }
                for row in grid.iter_mut().skip(cr + 1) {
                    for cell in row.iter_mut() {
                        *cell = Cell::BLANK;
                    }
                }
            }
            1 => {
                for row in grid.iter_mut().take(cr) {
                    for cell in row.iter_mut() {
                        *cell = Cell::BLANK;
                    }
                }
                if let Some(row) = grid.get_mut(cr) {
                    for cell in row.iter_mut().take(cc + 1) {
                        *cell = Cell::BLANK;
                    }
                }
            }
            _ => {
                for row in grid.iter_mut().take(rows) {
                    for cell in row.iter_mut().take(cols) {
                        *cell = Cell::BLANK;
                    }
                }
            }
        }
    }

    fn erase_line(&mut self, mode: u32) {
        let (cr, cc) = self.cursor;
        let cols = self.cols;
        let grid = self.grid_mut();
        if let Some(row) = grid.get_mut(cr) {
            match mode {
                0 => {
                    for cell in row.iter_mut().skip(cc) {
                        *cell = Cell::BLANK;
                    }
                }
                1 => {
                    for cell in row.iter_mut().take(cc + 1) {
                        *cell = Cell::BLANK;
                    }
                }
                _ => {
                    for cell in row.iter_mut().take(cols) {
                        *cell = Cell::BLANK;
                    }
                }
            }
        }
    }

    fn sgr(&mut self) {
        if self.params.is_empty() {
            self.params.push(0);
        }
        let mut i = 0;
        while i < self.params.len() {
            let v = self.params[i];
            match v {
                0 => self.pen = Pen::default(),
                1 => self.pen.bold = true,
                2 => {}
                4 => self.pen.underline = true,
                7 => self.pen.reverse = true,
                22 => self.pen.bold = false,
                24 => self.pen.underline = false,
                27 => self.pen.reverse = false,
                30..=37 => self.pen.fg = palette((v - 30) as u8),
                38 => {
                    if let Some(c) = self.sgr_ext_color(i + 1) {
                        self.pen.fg = c;
                        // 跳过扩展参数
                        match self.params.get(i + 1).copied().unwrap_or(0) {
                            5 => i += 2,
                            2 => i += 4,
                            _ => {}
                        }
                    }
                }
                39 => self.pen.fg = Color::DEF,
                40..=47 => self.pen.bg = palette((v - 40) as u8),
                48 => {
                    if let Some(c) = self.sgr_ext_color(i + 1) {
                        self.pen.bg = c;
                        match self.params.get(i + 1).copied().unwrap_or(0) {
                            5 => i += 2,
                            2 => i += 4,
                            _ => {}
                        }
                    }
                }
                49 => self.pen.bg = Color::DEF,
                90..=97 => self.pen.fg = palette((v - 90 + 8) as u8),
                100..=107 => self.pen.bg = palette((v - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    fn sgr_ext_color(&self, idx: usize) -> Option<Color> {
        match self.params.get(idx).copied().unwrap_or(0) {
            5 => Some(palette256(self.params.get(idx + 1).copied().unwrap_or(0) as u8)),
            2 => Some(Color::rgb(
                self.params.get(idx + 1).copied().unwrap_or(0) as u8,
                self.params.get(idx + 2).copied().unwrap_or(0) as u8,
                self.params.get(idx + 3).copied().unwrap_or(0) as u8,
            )),
            _ => None,
        }
    }

    fn full_reset(&mut self) {
        let (rows, cols) = (self.rows, self.cols);
        *self = Self::new(rows, cols);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_at(s: &Screen, r: usize, c: usize) -> Cell {
        s.cell(r, c)
    }

    fn text_line(s: &Screen, r: usize) -> String {
        (0..s.cols).map(|c| cell_at(s, r, c).ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn basic_text_and_newline() {
        let mut s = Screen::new(10, 40);
        s.feed(b"hello\r\nworld");
        assert_eq!(text_line(&s, 0), "hello");
        assert_eq!(text_line(&s, 1), "world");
        assert_eq!(s.cursor(), (1, 5));
    }

    #[test]
    fn utf8_multibyte_and_partial() {
        let mut s = Screen::new(4, 20);
        s.feed("你好".as_bytes());
        s.feed(&[0xE4]); // 半个"世"
        s.feed(&[0xB8, 0x96, 0x21]); // 世!
        assert_eq!(text_line(&s, 0), "你好世!");
    }

    #[test]
    fn sgr_colors() {
        let mut s = Screen::new(4, 40);
        s.feed(b"\x1b[31mred\x1b[0m plain");
        let red = cell_at(&s, 0, 0);
        assert_eq!(red.fg, palette(1));
        let plain = cell_at(&s, 0, 4);
        assert!(plain.fg.default);
        // 真彩
        s.feed(b"\x1b[38;2;10;20;30mX");
        let t = cell_at(&s, 0, 9);
        assert_eq!(t.fg, Color::rgb(10, 20, 30));
        // 256 色
        s.feed(b"\x1b[38;5;196mY");
        let y = cell_at(&s, 0, 10);
        assert_eq!(y.fg, palette256(196));
    }

    #[test]
    fn cursor_position_and_erase() {
        let mut s = Screen::new(10, 40);
        s.feed(b"AAAA\r\nBBBB\r\nCCCC");
        s.feed(b"\x1b[2;2H"); // 到 (1,1)
        s.feed(b"\x1b[K"); // 清行光标后 → BBBB 变 B
        assert_eq!(text_line(&s, 1), "B");
        s.feed(b"\x1b[2J"); // 全清
        assert_eq!(text_line(&s, 0), "");
        assert_eq!(text_line(&s, 2), "");
    }

    #[test]
    fn cursor_moves_wrap_bounds() {
        let mut s = Screen::new(5, 10);
        s.feed(b"\x1b[5;10HX"); // 末格,折行
        assert_eq!(s.cursor().0, 4);
        s.feed(b"\x1b[1;1H\x1b[3B\x1b[4C");
        assert_eq!(s.cursor(), (3, 4));
        s.feed(b"\x1b[100A"); // 上限保护
        assert_eq!(s.cursor().0, 0);
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut s = Screen::new(10, 40);
        s.feed(b"main");
        s.feed(b"\x1b[?1049h"); // 进 alt
        assert_eq!(text_line(&s, 0), "");
        s.feed(b"\x1b[31mTUI");
        assert_eq!(text_line(&s, 0), "TUI");
        s.feed(b"\x1b[?1049l"); // 出 alt
        assert_eq!(text_line(&s, 0), "main");
    }

    #[test]
    fn scroll_region_su_sd() {
        let mut s = Screen::new(6, 20);
        for i in 0..6 {
            s.feed(format!("L{}\r\n", i).as_bytes());
        }
        // 滚动区 2..=5(0 基),区内上滚一行
        s.feed(b"\x1b[2;6r\x1b[S");
        assert_eq!(text_line(&s, 0), "L1"); // 满屏 feed 末尾 CRLF 已把 L0 滚掉
        assert_eq!(text_line(&s, 1), "L3"); // 区内上滚:L2 滚出,区顶变 L3
    }

    #[test]
    fn osc_title() {
        let mut s = Screen::new(4, 20);
        s.feed(b"\x1b]0;my-agent - claude\x07ok");
        assert_eq!(s.title, "my-agent - claude");
        assert_eq!(text_line(&s, 0), "ok");
        // ST 结束变体
        s.feed(b"\x1b]2;title2\x1b\\");
        assert_eq!(s.title, "title2");
    }

    #[test]
    fn tab_and_backspace() {
        let mut s = Screen::new(4, 30);
        s.feed(b"a\tb");
        assert_eq!(text_line(&s, 0), "a       b");
        s.feed(b"\x08\x08X");
        assert_eq!(text_line(&s, 0), "a      Xb");
    }

    #[test]
    fn insert_delete_chars() {
        let mut s = Screen::new(4, 20);
        s.feed(b"ABCDEF");
        s.feed(b"\x1b[1;2H"); // (0,1)
        s.feed(b"\x1b[2@"); // 插 2 空格 → A  BCDEF
        assert_eq!(text_line(&s, 0), "A  BCDEF");
        s.feed(b"\x1b[3P"); // 删 3 → A EF? 实际 A + [space,space,B,C,D 被删] → "A EF"
        assert_eq!(text_line(&s, 0), "ACDEF");
    }

    #[test]
    fn reverse_video_cell() {
        let mut s = Screen::new(4, 20);
        s.feed(b"\x1b[7mR\x1b[27mn");
        assert!(cell_at(&s, 0, 0).reverse);
        assert!(!cell_at(&s, 0, 1).reverse);
    }

    #[test]
    fn cursor_visibility() {
        let mut s = Screen::new(4, 20);
        assert!(s.cursor_visible);
        s.feed(b"\x1b[?25l");
        assert!(!s.cursor_visible);
        s.feed(b"\x1b[?25h");
        assert!(s.cursor_visible);
    }
}
