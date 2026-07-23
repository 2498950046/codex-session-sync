import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ScanReport } from "./types";

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / 1024 ** index).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

export default function App() {
  const isTauriRuntime = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const [codexHome, setCodexHome] = useState("");
  const [report, setReport] = useState<ScanReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isTauriRuntime) {
      setCodexHome("~/.codex");
      return;
    }
    invoke<string>("get_default_codex_home")
      .then(setCodexHome)
      .catch((reason) => setError(String(reason)));
  }, [isTauriRuntime]);

  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);

  async function scan() {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<ScanReport>("scan_local_codex", {
        codexHome: codexHome.trim() || null,
      });
      setReport(result);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setLoading(false);
    }
  }

  return (
    <main className="app-shell">
      <header className="hero">
        <div>
          <span className="eyebrow">PHASE 1 · READ ONLY</span>
          <h1>Codex Session Sync</h1>
          <p>扫描本机 Codex 会话，建立安全同步所需的规范化 ThreadBundle。</p>
        </div>
        <div className="status-pill">
          {isTauriRuntime ? "本机数据不会被修改" : "浏览器预览模式"}
        </div>
      </header>

      <section className="panel scan-panel">
        <div className="field">
          <label htmlFor="codex-home">Codex Home</label>
          <input
            id="codex-home"
            value={codexHome}
            onChange={(event) => setCodexHome(event.target.value)}
            placeholder="~/.codex"
          />
        </div>
        <button
          onClick={scan}
          disabled={loading || !codexHome.trim() || !isTauriRuntime}
        >
          {loading
            ? "正在扫描…"
            : isTauriRuntime
              ? "扫描本机会话"
              : "桌面应用中可扫描"}
        </button>
      </section>

      {error && <div className="error-banner">{error}</div>}

      {report ? (
        <>
          <section className="metric-grid">
            <article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article>
            <article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article>
            <article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article>
            <article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article>
          </section>

          <section className="content-grid">
            <article className="panel">
              <div className="section-heading">
                <h2>会话预览</h2>
                <span>{report.threads.length} total</span>
              </div>
              <div className="thread-list">
                {recentThreads.map((thread) => (
                  <div className="thread-row" key={thread.threadId}>
                    <div>
                      <strong>{thread.title}</strong>
                      <span>{thread.workspace.sourcePath ?? "未记录工作目录"}</span>
                    </div>
                    <small>{thread.modelProvider ?? "unknown"}</small>
                  </div>
                ))}
              </div>
            </article>

            <article className="panel">
              <div className="section-heading">
                <h2>兼容性状态</h2>
                <span>{report.databasePaths.length} databases</span>
              </div>
              {report.warnings.length === 0 ? (
                <p className="success-copy">扫描完成，没有发现需要处理的兼容性问题。</p>
              ) : (
                <div className="warning-list">
                  {report.warnings.slice(0, 8).map((warning, index) => (
                    <div className="warning-row" key={`${warning.path}-${index}`}>
                      <strong>{warning.kind}</strong>
                      <span>{warning.message}</span>
                    </div>
                  ))}
                </div>
              )}
            </article>
          </section>
        </>
      ) : (
        <section className="empty-state">
          <div className="empty-icon">↻</div>
          <h2>等待第一次扫描</h2>
          <p>当前阶段只读取会话元数据、SQLite 索引和 rollout 哈希。</p>
        </section>
      )}
    </main>
  );
}
