//! Terminal replay journal(T3b,Issue #30;canonical spec §8.2/§8.3)。
//!
//! 内存有界 ring:脱敏后的 PTY 输出 chunk 在此分配 `terminal_epoch`
//! 内单调递增的 seq(从 1),供 replay 与 history gap 判定。PTY reader
//! 的 `append` 永不阻塞(超限驱逐最老数据),慢客户端反压在 per-client
//! 状态里隔离——绝不允许 ACK 缺席把 reader 或其它 client 拖停。
//!
//! durable transcript 与 WS transport 属后续 ticket(#32/#42);本模块
//! 只是输出侧数据面的进程内权威。

use std::collections::VecDeque;
use std::sync::Arc;

/// terminal epoch:一次真实 PTY 生命周期的标识。新 PTY 必须新 epoch,
/// 不得跨进程复用 seq(§8.3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalEpoch(uuid::Uuid);

impl TerminalEpoch {
    /// 生成新 epoch(UUIDv7,时间有序仅作诊断;唯一性是正确性来源)。
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl Default for TerminalEpoch {
    fn default() -> Self {
        Self::new()
    }
}

/// ring 中一个已分配 seq 的脱敏输出 chunk。
#[derive(Debug, Clone)]
pub struct JournalChunk {
    pub seq: u64,
    pub bytes: Arc<[u8]>,
}

/// history gap(§8.3):请求的 after_seq 已被 ring 驱逐。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "terminal_history_gap:after_seq={after_seq} first_available_seq={first_available_seq} last_seq={last_seq}"
)]
pub struct HistoryGap {
    pub after_seq: u64,
    pub first_available_seq: u64,
    pub last_seq: u64,
}

/// attach 请求与 journal 状态的校验问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachProblem {
    /// `after_seq > last_seq`:请求了尚不存在的输出(协议错误)。
    #[error("after_seq {requested} 超过 last_seq {last}")]
    AfterSeqBeyondLast { requested: u64, last: u64 },
    /// 请求的起点已被驱逐(§8.3 history gap;调用方须 4409 关闭并让
    /// Web 改读只读 transcript)。
    #[error(transparent)]
    HistoryGap(#[from] HistoryGap),
}

/// 会话 hello 投影(§8.1 `hello.v1` 的 journal 侧字段)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloFacts {
    pub terminal_epoch: TerminalEpoch,
    pub first_available_seq: u64,
    pub next_seq: u64,
    pub last_seq: u64,
}

/// 有界 replay journal。
pub struct TerminalJournal {
    epoch: TerminalEpoch,
    ring: VecDeque<JournalChunk>,
    ring_bytes: usize,
    max_ring_bytes: usize,
    /// 下一个将分配的 seq(空会话 = 1)。
    next_seq: u64,
    /// 已分配的最大 seq(空会话 = 0)。
    last_seq: u64,
}

impl TerminalJournal {
    pub fn new(max_ring_bytes: usize) -> Self {
        Self::with_epoch(TerminalEpoch::new(), max_ring_bytes)
    }

    /// 测试/恢复注入:以给定 epoch 建 journal(生产一律 `new`,新 PTY
    /// 必须新 epoch)。
    pub fn with_epoch(epoch: TerminalEpoch, max_ring_bytes: usize) -> Self {
        Self {
            epoch,
            ring: VecDeque::new(),
            ring_bytes: 0,
            max_ring_bytes: max_ring_bytes.max(1),
            next_seq: 1,
            last_seq: 0,
        }
    }

    pub fn epoch(&self) -> TerminalEpoch {
        self.epoch
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// ring 内最老可用 seq(空会话 = 1)。
    pub fn first_available_seq(&self) -> u64 {
        self.ring.front().map(|c| c.seq).unwrap_or(self.next_seq)
    }

    pub fn hello_facts(&self) -> HelloFacts {
        HelloFacts {
            terminal_epoch: self.epoch,
            first_available_seq: self.first_available_seq(),
            next_seq: self.next_seq,
            last_seq: self.last_seq,
        }
    }

    /// 追加脱敏输出并分配 seq。永不阻塞:容量超限时驱逐最老 chunk
    /// (`first_available_seq` 前移),PTY reader 不受任何客户端影响。
    /// 空 chunk 不占 seq(无字节即无输出消息)。
    pub fn append(&mut self, bytes: Vec<u8>) -> u64 {
        if bytes.is_empty() {
            return self.last_seq;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.last_seq = seq;
        let len = bytes.len();
        self.ring_bytes += len;
        self.ring.push_back(JournalChunk {
            seq,
            bytes: bytes.into(),
        });
        while self.ring_bytes > self.max_ring_bytes && self.ring.len() > 1 {
            if let Some(evicted) = self.ring.pop_front() {
                self.ring_bytes -= evicted.bytes.len();
            }
        }
        seq
    }

    /// 校验 attach 起点:`after_seq` 必须既不超过 `last_seq`,也不落在
    /// 已驱逐区间(§8.3)。通过后调用 `replay` 取增量。
    pub fn check_attach(&self, after_seq: u64) -> Result<HelloFacts, AttachProblem> {
        if after_seq > self.last_seq {
            return Err(AttachProblem::AfterSeqBeyondLast {
                requested: after_seq,
                last: self.last_seq,
            });
        }
        // after_seq == last_seq 表示"从现在开始";after_seq < first-1
        // 表示被驱逐(first_available_seq 之前的都不可用)。
        if after_seq + 1 < self.first_available_seq() {
            return Err(AttachProblem::HistoryGap(HistoryGap {
                after_seq,
                first_available_seq: self.first_available_seq(),
                last_seq: self.last_seq,
            }));
        }
        Ok(self.hello_facts())
    }

    /// 增量 replay:返回 seq > after_seq 的全部仍留存 chunk(同 epoch
    /// 覆盖内)。调用前必须先 `check_attach` 通过。
    pub fn replay(&self, after_seq: u64) -> Vec<JournalChunk> {
        self.ring
            .iter()
            .filter(|chunk| chunk.seq > after_seq)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_boundary_facts() {
        let journal = TerminalJournal::new(1024);
        let facts = journal.hello_facts();
        assert_eq!(
            (facts.next_seq, facts.last_seq, facts.first_available_seq),
            (1, 0, 1)
        );
        assert!(journal.replay(0).is_empty());
    }

    #[test]
    fn append_assigns_monotonic_seq_and_replays_incrementally() {
        let mut journal = TerminalJournal::new(64 * 1024);
        assert_eq!(journal.append(b"hello".to_vec()), 1);
        assert_eq!(journal.append(b"world".to_vec()), 2);
        assert_eq!(journal.hello_facts().next_seq, 3);
        let from_zero = journal.replay(0);
        assert_eq!(
            from_zero.iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let from_one = journal.replay(1);
        assert_eq!(from_one.len(), 1);
        assert_eq!(from_one[0].bytes.as_ref(), b"world");
    }

    #[test]
    fn eviction_moves_first_available_and_creates_gap() {
        let mut journal = TerminalJournal::new(16);
        journal.append(vec![0u8; 10]); // seq 1
        journal.append(vec![0u8; 10]); // seq 2 (ring_bytes=20 > 16 → 驱逐 seq1)
        assert_eq!(journal.first_available_seq(), 2);
        let gap = journal
            .check_attach(0)
            .expect_err("after_seq=0 应判 history gap");
        match gap {
            AttachProblem::HistoryGap(gap) => {
                assert_eq!(gap.first_available_seq, 2);
                assert_eq!(gap.last_seq, 2);
            }
            other => panic!("期望 gap,得到 {other:?}"),
        }
        // after_seq = first_available_seq - 1 仍可增量 replay
        journal.check_attach(1).expect("边界 attach 应通过");
        assert_eq!(journal.replay(1).len(), 1);
    }

    #[test]
    fn attach_beyond_last_seq_is_rejected() {
        let mut journal = TerminalJournal::new(1024);
        journal.append(b"x".to_vec());
        match journal.check_attach(5) {
            Err(AttachProblem::AfterSeqBeyondLast { requested, last }) => {
                assert_eq!((requested, last), (5, 1));
            }
            other => panic!("期望 AfterSeqBeyondLast,得到 {other:?}"),
        }
    }

    #[test]
    fn new_journal_gets_fresh_epoch() {
        let a = TerminalJournal::new(16);
        let b = TerminalJournal::new(16);
        assert_ne!(a.epoch(), b.epoch(), "新 PTY 必须新 epoch");
    }

    #[test]
    fn empty_append_does_not_consume_seq() {
        let mut journal = TerminalJournal::new(1024);
        journal.append(Vec::new());
        assert_eq!(journal.last_seq(), 0);
        assert_eq!(journal.next_seq(), 1);
    }
}
