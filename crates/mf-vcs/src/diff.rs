/// 统一 diff 解析:供 P4/Git 共用的渲染数据
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Header,
    HunkMeta,
    Context,
    Add,
    Delete,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// 显示文本(不含前缀 +/-)
    pub text: String,
    /// 行号(旧文件、新文件;上下文两者都有)
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct UnifiedDiff {
    pub lines: Vec<DiffLine>,
    /// 新增/删除行数统计
    pub added: usize,
    pub deleted: usize,
    /// 涉及文件(diff 头中的路径)
    pub files: Vec<String>,
}

impl UnifiedDiff {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

pub fn parse_unified_diff(text: &str) -> UnifiedDiff {
    let mut out = UnifiedDiff::default();
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            out.files
                .push(line[4..].split('\t').next().unwrap_or("").to_string());
            out.lines.push(DiffLine {
                kind: DiffLineKind::Header,
                text: line.to_string(),
                old_no: None,
                new_no: None,
            });
        } else if line.starts_with("@@") {
            // @@ -l,s +l,s @@
            let mut nums = parse_hunk_header(line);
            old_no = nums.next().unwrap_or(1);
            new_no = nums.next().unwrap_or(1);
            out.lines.push(DiffLine {
                kind: DiffLineKind::HunkMeta,
                text: line.to_string(),
                old_no: None,
                new_no: None,
            });
        } else if let Some(rest) = line.strip_prefix('+') {
            out.lines.push(DiffLine {
                kind: DiffLineKind::Add,
                text: rest.to_string(),
                old_no: None,
                new_no: Some(new_no),
            });
            new_no += 1;
            out.added += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            out.lines.push(DiffLine {
                kind: DiffLineKind::Delete,
                text: rest.to_string(),
                old_no: Some(old_no),
                new_no: None,
            });
            old_no += 1;
            out.deleted += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            out.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: rest.to_string(),
                old_no: Some(old_no),
                new_no: Some(new_no),
            });
            old_no += 1;
            new_no += 1;
        } else if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("==== ")
        {
            out.lines.push(DiffLine {
                kind: DiffLineKind::Header,
                text: line.to_string(),
                old_no: None,
                new_no: None,
            });
        } else if !line.is_empty() {
            // p4 diff 的上下文行有时无前导空格
            out.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: line.to_string(),
                old_no: Some(old_no),
                new_no: Some(new_no),
            });
            old_no += 1;
            new_no += 1;
        }
    }
    out
}

fn parse_hunk_header(line: &str) -> impl Iterator<Item = usize> + '_ {
    // 提取 "-a,b +c,d" 中的 a 与 c
    let mut nums = Vec::new();
    for part in line.split_whitespace() {
        if let Some(n) = part.strip_prefix('-') {
            if let Some(start) = n.split(',').next() {
                if let Ok(v) = start.parse::<usize>() {
                    nums.push(v);
                }
            }
        }
        if let Some(n) = part.strip_prefix('+') {
            if let Some(start) = n.split(',').next() {
                if let Ok(v) = start.parse::<usize>() {
                    nums.push(v);
                }
            }
        }
    }
    nums.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_unified() {
        let text = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,4 +1,5 @@\n fn main() {\n-    old();\n+    new();\n+    more();\n }\n";
        let d = parse_unified_diff(text);
        assert_eq!(d.added, 2);
        assert_eq!(d.deleted, 1);
        assert_eq!(d.files, vec!["a/foo.rs", "b/foo.rs"]);
        // 行号推进正确
        let add_line = d
            .lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Add)
            .unwrap();
        assert_eq!(add_line.new_no, Some(2));
        let del_line = d
            .lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Delete)
            .unwrap();
        assert_eq!(del_line.old_no, Some(2));
        let ctx_after = d
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Context)
            .last()
            .unwrap();
        assert_eq!(ctx_after.old_no, Some(3));
        assert_eq!(ctx_after.new_no, Some(4));
    }

    #[test]
    fn parse_empty_and_binary() {
        assert!(parse_unified_diff("").is_empty());
        let d = parse_unified_diff("diff -u file\nBinary files differ\n");
        assert!(d.files.is_empty());
    }
}
