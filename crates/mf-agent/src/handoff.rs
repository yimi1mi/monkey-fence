//! 结构化 Handoff(设计 §4.5):Agent Run 向下游提交的结果对象。
//!
//! 固定字段 + 自定义 `output` JSON;原始终端输出只通过 `raw_log_ref`
//! 引用,不复制完整会话转录。

use serde::{Deserialize, Serialize};

/// 结构化交接。适配器构造时用别名 `HandoffDraft`(同一类型)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    pub status: String,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub artifacts: Vec<String>,
    pub verification: Option<serde_json::Value>,
    pub blockers: Vec<String>,
    pub recommendations: Vec<String>,
    /// 插件或节点声明的自定义 JSON 输出。
    pub output: serde_json::Value,
    /// 原始终端输出的引用(日志文件路径/Run id),不是内容本身。
    pub raw_log_ref: Option<String>,
}
