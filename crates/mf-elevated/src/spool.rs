//! ACL 保护的 session spool(T3e,Issue #33;spec §10.3)。
//!
//! Core channel 断开后,root host 把**已脱敏**输出写入
//! `~/.monkeyfence/root-spool/<session>/`;容量受 `root_spool_max_bytes`
//! 硬上限约束(不越 cap)。新 Core 只读。真实 OS ACL(当前 logon SID
//! DACL / UDS 文件 ACL)属后续 ticket;fake seam 以当前用户目录 +
//! 显式只读标记实现语义。

use std::io::Write;
use std::path::PathBuf;

/// spool 写入问题。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpoolError {
    #[error("spool 容量超上限({needed} > {max})")]
    OverCapacity { needed: u64, max: u64 },
    #[error("spool IO 失败:{0}")]
    Io(#[from] std::io::Error),
}

/// 单会话 spool(fake 实现:目录 + 容量账本 + 只读标志)。
pub struct SessionSpool {
    dir: PathBuf,
    max_bytes: u64,
    written: u64,
    read_only: bool,
}

impl SessionSpool {
    /// 在 `root/<session>/` 下建 spool(fake 目录由调用方给定;
    /// 生产为 `~/.monkeyfence/root-spool/<session>/`)。
    pub fn create(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            max_bytes,
            written: 0,
            read_only: false,
        })
    }

    /// read-only reattach:只允许读取既有内容,任何写入失败关闭。
    pub fn attach_read_only(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        let written = Self::disk_bytes(&dir)?;
        Ok(Self {
            dir,
            max_bytes,
            written,
            read_only: true,
        })
    }

    fn disk_bytes(dir: &PathBuf) -> std::io::Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    /// 追加一段已脱敏输出;超容量 fail-closed(丢弃/截断由策略层决定,
    /// spool 自身绝不越 hard cap)。
    pub fn append(&mut self, chunk: &[u8]) -> Result<(), SpoolError> {
        if self.read_only {
            return Err(SpoolError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only reattach 不得写 spool",
            )));
        }
        let needed = self.written + chunk.len() as u64;
        if needed > self.max_bytes {
            return Err(SpoolError::OverCapacity {
                needed,
                max: self.max_bytes,
            });
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("output.log"))?;
        file.write_all(chunk)?;
        self.written = needed;
        Ok(())
    }

    /// 读取全部已持久化输出(read-only reattach / 导出)。
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.dir.join("output.log"))
    }

    pub fn written_bytes(&self) -> u64 {
        self.written
    }
}
