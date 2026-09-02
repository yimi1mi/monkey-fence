//! T3d 契约(Issue #32):transcript 存储——原子 commit、只读投影与
//! retention/cap GC(§3.2/§8.7)。仅临时库,不触用户目录。

use mf_agent::store::{Store, TranscriptSegment};
use mf_terminal::transcript::FINAL_STATE_LIVE;

fn temp_store() -> (tempfile::TempDir, std::sync::Arc<Store>) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("workflow-v1.db")).unwrap();
    (tmp, store)
}

fn commit_segment(store: &Store, handle: &str, epoch: &str, start: i64, bytes: &[u8]) {
    store
        .terminal_transcript_commit(
            handle,
            epoch,
            Some(TranscriptSegment {
                seq_start: start,
                seq_end: start,
                bytes,
            }),
            FINAL_STATE_LIVE,
            start,
            None,
            None,
        )
        .unwrap();
}

#[test]
fn segment_and_head_commit_atomically_and_project_readonly() {
    let (_tmp, store) = temp_store();
    commit_segment(&store, "sess-a", "epoch-1", 1, b"hello ");
    commit_segment(&store, "sess-a", "epoch-1", 2, b"redacted");
    // 纯状态收口(complete)
    store
        .terminal_transcript_mark("sess-a", "epoch-1", "complete", 2, Some(0), None)
        .unwrap();
    let view = store.terminal_transcript_view("sess-a").unwrap().unwrap();
    assert_eq!(view.head.final_state, "complete");
    assert_eq!(view.head.durable_through_seq, 2);
    assert_eq!(view.head.exit_code, Some(0));
    assert_eq!(view.segments.len(), 2);
    assert_eq!(view.segments[0].2, b"hello ");
    // 输入永不持久化:表里只有输出 segment(结构上无 input 表)。
    assert!(store
        .terminal_transcript_view("sess-missing")
        .unwrap()
        .is_none());
}

#[test]
fn retention_gc_removes_terminated_but_keeps_live_and_pinned() {
    let (_tmp, store) = temp_store();
    // live 会话:不清理
    commit_segment(&store, "sess-live", "e", 1, b"out");
    // 已终结会话(retention 到期由 cutoff 判定;retention_days=1 时
    // 刚写入的不到期——用 0 天边界由 max(1) 钳制,断言 live 永不清)
    store
        .terminal_transcript_mark("sess-done", "e", "complete", 1, Some(0), None)
        .unwrap();
    let removed = store
        .gc_terminal_transcripts(1, i64::MAX, &["sess-live"])
        .unwrap();
    // retention=1 天:刚写入的 complete 会话未到期,不清理
    assert_eq!(removed, 0);
    assert!(store
        .terminal_transcript_view("sess-live")
        .unwrap()
        .is_some());
    assert!(store
        .terminal_transcript_view("sess-done")
        .unwrap()
        .is_some());
    // pin 名单外且终态:极小 cap 触发 LRU 清理(sess-done)
    let removed_cap = store
        .gc_terminal_transcripts(90, 1, &["sess-live"])
        .unwrap();
    assert_eq!(removed_cap, 1, "cap 超限按 LRU 清已终结会话");
    assert!(
        store
            .terminal_transcript_view("sess-live")
            .unwrap()
            .is_some(),
        "live 永不清理"
    );
    assert!(store
        .terminal_transcript_view("sess-done")
        .unwrap()
        .is_none());
}

#[test]
fn idempotent_segment_recommit() {
    let (_tmp, store) = temp_store();
    commit_segment(&store, "sess-idem", "e", 1, b"same-bytes");
    commit_segment(&store, "sess-idem", "e", 1, b"same-bytes");
    let view = store
        .terminal_transcript_view("sess-idem")
        .unwrap()
        .unwrap();
    assert_eq!(view.segments.len(), 1, "同 seq 区间重复提交幂等");
}
