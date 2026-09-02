//! 三类安装 executor 与状态机(T4c,Issue #43)。
//!
//! `hardening` 承载共享的下载/argv/archive 硬化与统一执行流程;
//! 三类 executor 的差异在冻结计划的 kind 分派(`execute_plan`)中
//! 体现——package-manager 结构化直启、verified-download 冻结域/
//! digest/原子发布、custom-command 结构化模板。

pub mod hardening;

pub use hardening::{
    execute_plan, validate_archive_entry, validate_download_url, validate_structured_argv,
    DownloadPolicy, ExecuteOutcome, ExecutorEnv, JobPhase, ProgressEvent,
};
