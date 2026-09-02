/// T3f(Issue #34):transcript durable sink——按 session→project root
/// 路由到对应 Project Store(`terminal_transcript_commit`),Store 句柄
/// LRU 缓存;失败只记日志,绝不回压 PTY reader(§8.2)。
struct StoreTranscriptSink {
    /// project root → Store(project_db_path);上限 LRU。
    stores: parking_lot::Mutex<Vec<(PathBuf, Arc<Store>)>>,
}

const SINK_STORE_CACHE: usize = 8;

impl StoreTranscriptSink {
    fn store_for(&self, root: &Path) -> Option<Arc<Store>> {
        {
            let mut cache = self.stores.lock();
            if let Some(pos) = cache.iter().position(|(path, _)| path == root) {
                let entry = cache.remove(pos);
                cache.push(entry.clone());
                return Some(entry.1);
            }
        }
        let store = Store::open(&mf_agent::project_db_path(root)).ok()?;
        let mut cache = self.stores.lock();
        if cache.len() >= SINK_STORE_CACHE {
            cache.remove(0);
        }
        cache.push((root.to_path_buf(), store.clone()));
        Some(store)
    }
}

impl crate::runtime_host::TranscriptSink for StoreTranscriptSink {
    fn commit(
        &self,
        session_handle: &str,
        project_root: Option<&Path>,
        epoch: &str,
        batch: Option<&mf_terminal::transcript::FlushBatch>,
        final_state: &str,
        durable_through_seq: u64,
        exit_code: Option<i64>,
    ) {
        let Some(root) = project_root else {
            log::warn!("transcript 会话 {session_handle} 无 project 路由,批次丢弃");
            return;
        };
        let Some(store) = self.store_for(root) else {
            log::warn!(
                "transcript 会话 {session_handle} 的 Store 打开失败({}),批次丢弃",
                root.display()
            );
            return;
        };
        let result = store.terminal_transcript_commit(
            session_handle,
            epoch,
            batch.map(|b| mf_agent::store::TranscriptSegment {
                seq_start: b.seq_start as i64,
                seq_end: b.seq_end as i64,
                bytes: &b.bytes,
            }),
            final_state,
            durable_through_seq as i64,
            exit_code,
            None,
        );
        if let Err(error) = result {
            // durable 失败只记日志:reader 不回压;exit 门闩的失败语义
            // 由调用方(生产 exit 链)另行处理。
            log::error!("transcript flush 失败({session_handle}):{error:#}");
        }
    }
}
