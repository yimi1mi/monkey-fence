use anyhow::{Context, Result};
use ropey::Rope;
use std::io;
use std::path::{Path, PathBuf};

/// 一次原子编辑(替换 [start, end) 字节区间为 text)
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// 事务 = 一组按序应用的编辑;undo 时逆序应用逆编辑
#[derive(Clone, Debug)]
pub struct Transaction {
    edits: Vec<Edit>,
}

impl Transaction {
    fn invert_for(&self, old_text: &str) -> Vec<Edit> {
        // 事务内编辑按 start 升序且互不重叠时,逆编辑可由旧文本精确重建;
        // 逆编辑的偏移需按此前编辑造成的长度差(delta)校正到新文本坐标系
        let mut inverted = Vec::with_capacity(self.edits.len());
        let mut delta: i64 = 0;
        for e in &self.edits {
            let original = &old_text.as_bytes()[e.start..e.end];
            // Safety: 编辑保证落在 UTF-8 边界上
            let original = String::from_utf8_lossy(original).into_owned();
            let inv_start = (e.start as i64 + delta) as usize;
            delta += e.text.len() as i64 - (e.end - e.start) as i64;
            inverted.push(Edit {
                start: inv_start,
                end: inv_start + e.text.len(),
                text: original,
            });
        }
        inverted
    }
}

/// 文本缓冲:rope 存储,字节偏移寻址,行号为 0 起
pub struct Buffer {
    path: Option<PathBuf>,
    rope: Rope,
    dirty: bool,
    version: u64,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    /// 外部修改检测用的时间戳(上次读/写)
    loaded_mtime: Option<std::time::SystemTime>,
}

impl Buffer {
    pub fn empty(path: Option<PathBuf>) -> Self {
        Self {
            path,
            rope: Rope::new(),
            dirty: false,
            version: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            loaded_mtime: None,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        // 去除 BOM
        let bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            bytes[3..].to_vec()
        } else {
            bytes
        };
        let text = String::from_utf8(bytes.clone())
            .with_context(|| format!("file {} is not valid UTF-8", path.display()))?;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut buf = Self::empty(Some(path.to_path_buf()));
        buf.rope = Rope::from_str(&text);
        buf.loaded_mtime = mtime;
        Ok(buf)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines().max(1)
    }

    /// 行内容(不含换行符)
    pub fn line(&self, row: usize) -> String {
        if row >= self.rope.len_lines() {
            return String::new();
        }
        let mut s = self.rope.line(row).to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        if s.ends_with('\r') {
            s.pop();
        }
        s
    }

    /// 行的字节区间 [start, end) —— end 不含换行符
    pub fn line_byte_range(&self, row: usize) -> (usize, usize) {
        let start = self.rope.line_to_byte(row);
        let end = if row + 1 >= self.len_lines() {
            self.rope.len_bytes()
        } else {
            let next = self.rope.line_to_byte(row + 1);
            let mut e = next;
            // 去掉行尾换行
            if e > start {
                let prev = self.byte_at(e - 1);
                if prev == b'\n' {
                    e -= 1;
                    if e > start && self.byte_at(e - 1) == b'\r' {
                        e -= 1;
                    }
                }
            }
            e
        };
        (start, end)
    }

    fn byte_at(&self, i: usize) -> u8 {
        self.rope.byte(i)
    }

    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        self.rope.char_to_byte(char_idx.min(self.rope.len_chars()))
    }

    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        self.rope.byte_to_char(byte_idx.min(self.rope.len_bytes()))
    }

    /// 字节偏移 → (行, 列);列按字符计
    pub fn offset_to_pos(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.rope.len_bytes());
        let row = self.rope.byte_to_line(offset);
        let line_start = self.rope.line_to_byte(row);
        let col = self.rope.byte_to_char(offset) - self.rope.byte_to_char(line_start);
        (row, col)
    }

    /// (行, 字符列) → 字节偏移
    pub fn pos_to_offset(&self, row: usize, col: usize) -> usize {
        let row = row.min(self.rope.len_lines().saturating_sub(1));
        let line_start = self.rope.line_to_byte(row);
        let line_len_chars = self.rope.line(row).len_chars();
        let col = col.min(line_len_chars.saturating_sub(if line_len_chars > 0 { 1 } else { 0 }));
        let target = self.rope.byte_to_char(line_start) + col;
        self.rope.char_to_byte(target)
    }

    /// 行首(指定行)的字节偏移
    pub fn line_start_offset(&self, row: usize) -> usize {
        self.rope.line_to_byte(row.min(self.rope.len_lines()))
    }

    /// 应用事务(编辑需按升序且互不重叠);返回是否成功
    pub fn apply(&mut self, edits: Vec<Edit>) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        let old_text = self.text();
        let mut valid = true;
        for w in edits.windows(2) {
            if w[0].end > w[1].start {
                valid = false;
            }
        }
        if !valid {
            anyhow::bail!("transaction edits must be ordered and non-overlapping");
        }
        let max = self.rope.len_bytes();
        if edits.iter().any(|e| e.start > e.end || e.end > max) {
            anyhow::bail!("edit range out of bounds");
        }
        // 逆序应用,使前面的偏移不受影响
        for e in edits.iter().rev() {
            if e.end > e.start {
                let (s, e2) = (self.char_index(e.start), self.char_index(e.end));
                self.rope.remove(s..e2);
            }
            if !e.text.is_empty() {
                self.rope.insert(self.char_index(e.start), &e.text);
            }
        }
        let tx = Transaction { edits };
        let inverse = Transaction {
            edits: tx.invert_for(&old_text),
        };
        self.undo_stack.push(inverse);
        self.redo_stack.clear();
        self.dirty = true;
        self.version += 1;
        Ok(())
    }

    fn char_index(&self, byte_idx: usize) -> usize {
        self.rope.byte_to_char(byte_idx)
    }

    pub fn undo(&mut self) -> bool {
        if let Some(tx) = self.undo_stack.pop() {
            let old_text = self.text();
            let redo_tx = Transaction {
                edits: tx.invert_for(&old_text),
            };
            for e in tx.edits.iter().rev() {
                if e.end > e.start {
                    let (s, e2) = (self.char_index(e.start), self.char_index(e.end));
                    self.rope.remove(s..e2);
                }
                if !e.text.is_empty() {
                    self.rope.insert(self.char_index(e.start), &e.text);
                }
            }
            self.redo_stack.push(redo_tx);
            self.dirty = true;
            self.version += 1;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(tx) = self.redo_stack.pop() {
            let old_text = self.text();
            let undo_tx = Transaction {
                edits: tx.invert_for(&old_text),
            };
            for e in tx.edits.iter().rev() {
                if e.end > e.start {
                    let (s, e2) = (self.char_index(e.start), self.char_index(e.end));
                    self.rope.remove(s..e2);
                }
                if !e.text.is_empty() {
                    self.rope.insert(self.char_index(e.start), &e.text);
                }
            }
            self.undo_stack.push(undo_tx);
            self.dirty = true;
            self.version += 1;
            true
        } else {
            false
        }
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(path) = &self.path {
            let tmp = path.with_extension("mf-tmp");
            std::fs::write(&tmp, self.rope.to_string())
                .with_context(|| format!("write {}", tmp.display()))?;
            // 原子替换,避免崩溃留下半截文件
            std::fs::rename(&tmp, path)
                .or_else(|_| std::fs::copy(&tmp, path).map(|_| ()))
                .with_context(|| format!("rename to {}", path.display()))?;
            let _ = std::fs::remove_file(&tmp);
            self.dirty = false;
            self.loaded_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            Ok(())
        } else {
            anyhow::bail!("buffer has no path")
        }
    }

    /// 磁盘上文件是否比缓冲新(外部修改)
    pub fn changed_on_disk(&self) -> bool {
        match (&self.path, &self.loaded_mtime) {
            (Some(p), Some(loaded)) => std::fs::metadata(p)
                .and_then(|m| m.modified())
                .map(|cur| cur > *loaded)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// 从磁盘重新加载(保留 undo 历史)
    pub fn reload_from_disk(&mut self) -> Result<()> {
        if let Some(path) = self.path.clone() {
            let fresh = Self::load(&path)?;
            self.rope = fresh.rope;
            self.loaded_mtime = fresh.loaded_mtime;
            self.version += 1;
            self.dirty = false;
            Ok(())
        } else {
            anyhow::bail!("buffer has no path")
        }
    }

    pub fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            text: self.rope.clone(),
            version: self.version,
        }
    }
}

/// 不可变快照,供后台线程(高亮/搜索)使用,不阻塞 UI
#[derive(Clone)]
pub struct BufferSnapshot {
    pub text: Rope,
    pub version: u64,
}

impl BufferSnapshot {
    pub fn as_str_lines(&self) -> impl Iterator<Item = String> + '_ {
        (0..self.text.len_lines()).map(move |i| self.text.line(i).to_string())
    }
}

pub fn read_to_string_limit(path: &Path, limit: u64) -> io::Result<Option<String>> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > limit {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None), // 二进制文件
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        let mut b = Buffer::empty(None);
        b.rope = Rope::from_str(s);
        b
    }

    #[test]
    fn edit_insert_delete() {
        let mut b = buf("hello world");
        b.apply(vec![Edit {
            start: 5,
            end: 5,
            text: ",".into(),
        }])
        .unwrap();
        assert_eq!(b.text(), "hello, world");
        b.apply(vec![Edit {
            start: 5,
            end: 6,
            text: "".into(),
        }])
        .unwrap();
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut b = buf("fn main() {}\n");
        b.apply(vec![Edit {
            start: 11,
            end: 11,
            text: "\n    println!(\"hi\");".into(),
        }])
        .unwrap();
        assert!(b.text().contains("println"));
        assert!(b.undo());
        assert_eq!(b.text(), "fn main() {}\n");
        assert!(b.redo());
        assert!(b.text().contains("println"));
        assert!(b.undo());
        assert!(b.undo() == false);
    }

    #[test]
    fn multi_edit_transaction_undo() {
        let mut b = buf("one two three");
        b.apply(vec![
            Edit {
                start: 0,
                end: 3,
                text: "1".into(),
            },
            Edit {
                start: 4,
                end: 7,
                text: "2".into(),
            },
            Edit {
                start: 8,
                end: 13,
                text: "3".into(),
            },
        ])
        .unwrap();
        assert_eq!(b.text(), "1 2 3");
        b.undo();
        assert_eq!(b.text(), "one two three");
    }

    #[test]
    fn positions() {
        let b = buf("ab\ncd\nef");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.offset_to_pos(3), (1, 0));
        assert_eq!(b.offset_to_pos(4), (1, 1));
        assert_eq!(b.pos_to_offset(1, 1), 4);
        assert_eq!(b.line(0), "ab");
        assert_eq!(b.line(2), "ef");
        assert_eq!(b.line_byte_range(1), (3, 5));
    }

    #[test]
    fn utf8_offsets() {
        let b = Buffer {
            rope: Rope::from_str("你好\nworld"),
            ..Buffer::empty(None)
        };
        assert_eq!(b.offset_to_pos(7), (1, 0));
        assert_eq!(b.line(1), "world");
    }

    #[test]
    fn reject_overlapping() {
        let mut b = buf("abcdef");
        assert!(b
            .apply(vec![
                Edit {
                    start: 0,
                    end: 3,
                    text: "x".into()
                },
                Edit {
                    start: 2,
                    end: 4,
                    text: "y".into()
                },
            ])
            .is_err());
    }
}
