use std::path::Path;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter};

/// 高亮语义标签(自有 scope 体系,不依赖 TextMate 主题)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightTag {
    Comment,
    String,
    Keyword,
    Number,
    Function,
    Type,
    Variable,
    Constant,
    Property,
    Operator,
    Punctuation,
    Tag,
    Attribute,
}

/// scope 选择器表:索引即 Highlight 编号
const SCOPES: &[(&str, HighlightTag)] = &[
    ("comment", HighlightTag::Comment),
    ("string", HighlightTag::String),
    ("keyword", HighlightTag::Keyword),
    ("constant.numeric", HighlightTag::Number),
    ("entity.name.function", HighlightTag::Function),
    ("entity.name.type", HighlightTag::Type),
    ("entity.name.class", HighlightTag::Type),
    ("entity.name.struct", HighlightTag::Type),
    ("entity.name.enum", HighlightTag::Type),
    ("support.type", HighlightTag::Type),
    ("support.class", HighlightTag::Type),
    ("variable", HighlightTag::Variable),
    ("constant", HighlightTag::Constant),
    ("variable.other.property", HighlightTag::Property),
    ("variable.other.member", HighlightTag::Property),
    ("keyword.operator", HighlightTag::Operator),
    ("punctuation", HighlightTag::Punctuation),
    ("entity.name.tag", HighlightTag::Tag),
    ("attribute", HighlightTag::Attribute),
];

/// 一个高亮片段:字节区间 [start, end)
#[derive(Clone, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub tag: HighlightTag,
}

pub struct LangConfig {
    pub name: &'static str,
    config: HighlightConfiguration,
}

/// 按扩展名挑选语言并构建高亮配置
pub fn config_for_path(path: &Path) -> Option<LangConfig> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (name, mut config) = match ext.as_str() {
        "rs" => rust_config().map(|c| ("rust", c)),
        "json" => json_config().map(|c| ("json", c)),
        "py" => python_config().map(|c| ("python", c)),
        "js" | "mjs" | "cjs" => javascript_config().map(|c| ("javascript", c)),
        "ts" => typescript_config().map(|c| ("typescript", c)),
        "tsx" => tsx_config().map(|c| ("tsx", c)),
        "c" | "h" => c_config().map(|c| ("c", c)),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => cpp_config().map(|c| ("cpp", c)),
        _ => return None,
    }?;
    let scopes: Vec<String> = SCOPES.iter().map(|(s, _)| (*s).to_string()).collect();
    config.configure(&scopes);
    Some(LangConfig { name, config })
}

/// 对整段文本做高亮,返回按 start 升序、互不重叠的片段列表
pub fn highlight(text: &str, lang: &LangConfig) -> Vec<Span> {
    let mut hl = TsHighlighter::new();
    let Ok(events) = hl.highlight(&lang.config, text.as_bytes(), None, |_| None) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut cur_tag: Option<HighlightTag> = None;
    for ev in events.flatten() {
        match ev {
            HighlightEvent::Source { start, end } => {
                if let Some(tag) = cur_tag {
                    if end > start {
                        spans.push(Span { start, end, tag });
                    }
                }
            }
            HighlightEvent::HighlightStart(h) => {
                cur_tag = SCOPES.get(h.0).map(|(_, t)| *t);
            }
            HighlightEvent::HighlightEnd => {
                cur_tag = None;
            }
        }
    }
    spans
}

/// 把字节区间片段切分为逐行片段,便于按行渲染
/// line_starts: 每行行首字节偏移(升序)
pub fn spans_by_line(spans: &[Span], line_starts: &[usize]) -> Vec<Vec<(usize, usize, HighlightTag)>> {
    let mut out: Vec<Vec<(usize, usize, HighlightTag)>> = vec![Vec::new(); line_starts.len()];
    for s in spans {
        // 找到起始行
        let mut row = line_starts.partition_point(|&ls| ls <= s.start).saturating_sub(1);
        let mut pos = s.start;
        while pos < s.end && row < line_starts.len() {
            let line_end = line_starts
                .get(row + 1)
                .copied()
                .unwrap_or(usize::MAX);
            let seg_end = s.end.min(line_end);
            if seg_end > pos {
                out[row].push((pos - line_starts[row], seg_end - line_starts[row], s.tag));
            }
            pos = seg_end;
            row += 1;
        }
    }
    out
}

fn rust_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        tree_sitter_rust::INJECTIONS_QUERY,
        "",
    )
    .ok()
}

fn json_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_json::LANGUAGE.into(),
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
}

fn python_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_python::LANGUAGE.into(),
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
}

fn javascript_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_javascript::LANGUAGE.into(),
        "javascript",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::INJECTIONS_QUERY,
        "",
    )
    .ok()
}

fn typescript_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescript",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
}

fn tsx_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        "tsx",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
}

fn c_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_c::LANGUAGE.into(),
        "c",
        tree_sitter_c::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .ok()
}

fn cpp_config() -> Option<HighlightConfiguration> {
    HighlightConfiguration::new(
        tree_sitter_cpp::LANGUAGE.into(),
        "cpp",
        tree_sitter_cpp::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_highlight_produces_spans() {
        let cfg = config_for_path(Path::new("main.rs")).expect("rust config");
        let spans = highlight("fn main() { let x = 1; }", &cfg);
        assert!(!spans.is_empty());
        // 所有 span 按升序且不重叠
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start, "spans must be ordered: {:?}", spans);
        }
    }

    #[test]
    fn unknown_ext_no_config() {
        assert!(config_for_path(Path::new("data.xyz")).is_none());
    }

    #[test]
    fn line_splitting() {
        let cfg = config_for_path(Path::new("a.rs")).unwrap();
        let text = "let a = 1;\nlet b = 2;";
        let spans = highlight(text, &cfg);
        let line_starts = vec![0, 11];
        let by_line = spans_by_line(&spans, &line_starts);
        assert_eq!(by_line.len(), 2);
        for spans in &by_line {
            for (s, e, _) in spans {
                assert!(*e <= 11 || *s >= 11);
            }
        }
    }
}
