//! 安装与 Provider 数值上限(canonical spec 附录 A5;T4b,Issue #40)。

/// 可配置的 installer/provider 上限。安装执行侧参数(plan ttl/job
/// timeout/下载边界)为 #43 冻结;discovery/provider 部分本 ticket 生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallerLimits {
    /// 每 candidate 命令探测超时。
    pub discovery_probe_timeout_ms: u64,
    /// Provider Type 级模型缓存 TTL。
    pub provider_model_cache_ttl_secs: u64,
    /// 远端模型目录探测超时。
    pub provider_probe_timeout_ms: u64,
    /// 探测重试次数(退避 500ms/2000ms ±20% 抖动,fixed 序列)。
    pub provider_probe_retries: u32,
    /// 冻结安装计划 TTL(#43)。
    pub install_plan_ttl_secs: u64,
    /// 安装 job 超时(#43)。
    pub install_job_timeout_ms: u64,
    /// 协作取消后强杀宽限(#43)。
    pub install_cancel_kill_grace_ms: u64,
    /// 每 artifact 下载上限(#43)。
    pub install_download_max_bytes: u64,
    /// 解压后 archive 上限(#43)。
    pub install_archive_max_bytes: u64,
    /// 下载无字节停滞超时(#43)。
    pub install_download_stall_timeout_ms: u64,
    /// 脱敏后 job journal 字节上限(#43)。
    pub install_output_max_bytes: u64,
    /// 脱敏后 job journal 行上限(#43)。
    pub install_output_max_lines: u64,
}

/// `install_redirect_max` fixed = 5(仅限预览冻结域名;附录 A5)。
pub const INSTALL_REDIRECT_MAX: u32 = 5;

impl Default for InstallerLimits {
    fn default() -> Self {
        Self {
            discovery_probe_timeout_ms: 5_000,
            provider_model_cache_ttl_secs: 300,
            provider_probe_timeout_ms: 10_000,
            provider_probe_retries: 2,
            install_plan_ttl_secs: 600,
            install_job_timeout_ms: 1_800_000,
            install_cancel_kill_grace_ms: 60_000,
            install_download_max_bytes: 2 * 1024 * 1024 * 1024,
            install_archive_max_bytes: 4 * 1024 * 1024 * 1024,
            install_download_stall_timeout_ms: 60_000,
            install_output_max_bytes: 2 * 1024 * 1024,
            install_output_max_lines: 10_000,
        }
    }
}

impl InstallerLimits {
    /// 越界钳制到附录 A5 允许范围;retries hard cap 3。
    pub fn clamp(&self) -> Self {
        Self {
            discovery_probe_timeout_ms: self.discovery_probe_timeout_ms.clamp(1_000, 30_000),
            provider_model_cache_ttl_secs: self.provider_model_cache_ttl_secs.clamp(60, 86_400),
            provider_probe_timeout_ms: self.provider_probe_timeout_ms.clamp(2_000, 30_000),
            provider_probe_retries: self.provider_probe_retries.min(3),
            install_plan_ttl_secs: self.install_plan_ttl_secs.clamp(60, 3_600),
            install_job_timeout_ms: self.install_job_timeout_ms.clamp(300_000, 7_200_000),
            install_cancel_kill_grace_ms: self.install_cancel_kill_grace_ms.clamp(10_000, 300_000),
            install_download_max_bytes: self
                .install_download_max_bytes
                .clamp(16 * 1024 * 1024, 8 * 1024 * 1024 * 1024),
            install_archive_max_bytes: self
                .install_archive_max_bytes
                .clamp(64 * 1024 * 1024, 16 * 1024 * 1024 * 1024),
            install_download_stall_timeout_ms: self
                .install_download_stall_timeout_ms
                .clamp(10_000, 300_000),
            install_output_max_bytes: self
                .install_output_max_bytes
                .clamp(256 * 1024, 16 * 1024 * 1024),
            install_output_max_lines: self.install_output_max_lines.clamp(1_000, 100_000),
        }
    }

    /// 固定退避序列(500ms、2000ms ±20% 抖动;附录 A5)。`attempt` 从 0
    /// 起(首次重试前的等待)。抖动由调用方注入(生产用熵源,测试传 0)。
    pub fn provider_backoff_ms(&self, attempt: u32, jitter_fraction: f64) -> u64 {
        let base = match attempt {
            0 => 500u64,
            _ => 2_000,
        };
        let jitter = (base as f64 * jitter_fraction.clamp(-0.2, 0.2)) as i64;
        (base as i64 + jitter).max(0) as u64
    }
}
