// 代码浏览与版控面板(#80/#81):目录树浏览(fs/dirs + fs/file 只读)
// 与 git 状态(mf-vcs 只读;stage/commit 等写操作留后续)。
// #multi-folder:项目含多个文件夹时顶部提供切换(primary 恒为第一项)。

import { useCallback, useEffect, useState } from "react";
import type { WorkbenchClient } from "../api/client.ts";

/** 多文件夹切换器:单文件夹时不渲染。value 按前缀匹配当前路径所属
 *  文件夹(浏览进入子目录后仍保持选中)。 */
function FolderSwitcher({
  folders,
  current,
  onSwitch,
  label,
}: {
  folders?: string[];
  current: string;
  onSwitch: (path: string) => void;
  label: string;
}) {
  if (!folders || folders.length < 2) return null;
  const selected =
    folders.find(
      (path) => current === path || current.startsWith(path + "\\") || current.startsWith(path + "/"),
    ) ?? "";
  return (
    <div className="field folder-filter">
      <select
        aria-label={label}
        value={selected}
        onChange={(event) => onSwitch(event.target.value)}
      >
        {folders.map((path, index) => (
          <option key={path} value={path}>
            {index === 0 ? "主 " : "附 "}
            {path.split(/[\\/]/).pop() || path}({path})
          </option>
        ))}
      </select>
    </div>
  );
}

export function CodeBrowserModal({
  client,
  startPath,
  title,
  folders,
  onClose,
}: {
  client: WorkbenchClient;
  startPath: string;
  title: string;
  folders?: string[];
  onClose: () => void;
}) {
  const [current, setCurrent] = useState(startPath);
  const [entries, setEntries] = useState<Array<{ path: string; name: string }>>([]);
  const [parent, setParent] = useState<string | null>(null);
  const [file, setFile] = useState<{ path: string; content: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const openDir = useCallback(
    async (path: string) => {
      setLoading(true);
      setError(null);
      setFile(null);
      try {
        const listing = await client.fsDirs(path);
        setCurrent(listing.path);
        setParent(listing.parent);
        setEntries(listing.dirs);
        if (listing.error) setError(listing.error);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setLoading(false);
      }
    },
    [client],
  );

  useEffect(() => {
    void openDir(startPath);
  }, [openDir, startPath]);

  const readFile = async (input: string) => {
    // fs/dirs 只列目录;文件浏览通过 fs/file 直接按完整路径读
    setError(null);
    const response = await fetch(`/api/v1/fs/file?path=${encodeURIComponent(input)}`);
    const data = (await response.json()) as { content?: string; message?: string };
    if (!response.ok) {
      setError(data.message ?? `读取失败 HTTP ${response.status}`);
      return;
    }
    setFile({ path: input, content: data.content ?? "" });
  };

  return (
    <div
      className="scrim"
      // 弹窗只经显式关闭退出(误触遮罩不丢失浏览位置)
      >
      <div className="modal folder-modal" role="dialog" aria-modal="true" aria-label="代码浏览">
        <h3>
          <span className="mark">▤</span>代码浏览 · {title}
        </h3>
        <FolderSwitcher
          folders={folders}
          current={current}
          onSwitch={(path) => void openDir(path)}
          label="切换项目文件夹"
        />
        <div className="folder-breadcrumb">
          <button className="crumb" disabled={!parent} onClick={() => parent && openDir(parent)}>
            ↑
          </button>
          <span className="crumb-seg">{current}</span>
          {loading && <span className="crumb-loading">读取中…</span>}
        </div>
        <div className="folder-list">
          {entries.length === 0 && !loading && (
            <p className="muted-note">{error ?? "没有子目录。"}</p>
          )}
          {entries.map((entry) => (
            <button
              key={entry.path}
              className="folder-item"
              onClick={() => openDir(entry.path)}
            >
              <span className="folder-icon">▸</span>
              {entry.name}
            </button>
          ))}
        </div>
        <div className="field folder-filter">
          <input
            placeholder="按完整路径读取文件(≤256KB 文本)"
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                const value = (event.target as HTMLInputElement).value.trim();
                if (value) void readFile(value);
              }
            }}
          />
        </div>
        {file && (
          <pre className="file-view">
            <div className="file-view-path">{file.path}</div>
            {file.content.slice(0, 10000)}
            {file.content.length > 10000 && "\n…(截断展示)"}
          </pre>
        )}
        <div className="actions">
          <button className="mf-btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

export function VcsPanel({
  root,
  folders,
  onClose,
}: {
  root: string;
  folders?: string[];
  onClose: () => void;
}) {
  const [active, setActive] = useState(root);
  const [status, setStatus] = useState<{
    repo: boolean;
    branch?: string;
    entries?: Array<{ path: string; status: string }>;
  } | null>(null);

  useEffect(() => {
    void (async () => {
      const response = await fetch(`/api/v1/vcs/status?root=${encodeURIComponent(active)}`);
      if (response.ok) setStatus(await response.json());
    })();
  }, [active]);

  return (
    <div
      className="scrim"
      // 弹窗只经显式关闭退出(误触遮罩不丢失浏览位置)
      >
      <div className="modal folder-modal" role="dialog" aria-modal="true" aria-label="版控状态">
        <h3>
          <span className="mark">⑂</span>版控 · {active}
        </h3>
        <FolderSwitcher
          folders={folders}
          current={active}
          onSwitch={setActive}
          label="切换项目文件夹"
        />
        {!status ? (
          <p className="muted-note">读取中…</p>
        ) : !status.repo ? (
          <p className="muted-note">此目录不是 git 仓库。</p>
        ) : (
          <>
            <p className="muted-note">
              分支 <strong>{status.branch}</strong> · {status.entries?.length ?? 0} 个变更
              (只读;stage/commit 留后续)
            </p>
            <div className="folder-list">
              {(status.entries ?? []).map((entry) => (
                <div key={entry.path} className="folder-item">
                  <span className="mono-dim">{entry.status}</span>
                  {entry.path}
                </div>
              ))}
              {(status.entries?.length ?? 0) === 0 && (
                <p className="muted-note">工作区干净。</p>
              )}
            </div>
          </>
        )}
        <div className="actions">
          <button className="mf-btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
