import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  ImportReport,
  OperationJournal,
  ScanReport,
  SnapshotSummary,
  SnapshotValidationReport,
} from "./types";

type Action = "scan" | "snapshot" | "validate" | "import" | "recover" | null;

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    units.length - 1,
  );
  return `${(bytes / 1024 ** index).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

export default function App() {
  const isTauriRuntime =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const [codexHome, setCodexHome] = useState("");
  const [repositoryRoot, setRepositoryRoot] = useState("");
  const [manifestPath, setManifestPath] = useState("");
  const [journalPath, setJournalPath] = useState("");
  const [confirmedClosed, setConfirmedClosed] = useState(false);
  const [report, setReport] = useState<ScanReport | null>(null);
  const [snapshot, setSnapshot] = useState<SnapshotSummary | null>(null);
  const [validation, setValidation] =
    useState<SnapshotValidationReport | null>(null);
  const [importReport, setImportReport] = useState<ImportReport | null>(null);
  const [recoveredJournal, setRecoveredJournal] =
    useState<OperationJournal | null>(null);
  const [action, setAction] = useState<Action>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isTauriRuntime) {
      setCodexHome("~/.codex");
      setRepositoryRoot("~/.codex-session-sync");
      return;
    }
    Promise.all([
      invoke<string>("get_default_codex_home"),
      invoke<string>("get_default_repository_root"),
    ])
      .then(([home, repository]) => {
        setCodexHome(home);
        setRepositoryRoot(repository);
      })
      .catch((reason) => setError(String(reason)));
  }, [isTauriRuntime]);

  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);
  const busy = action !== null;

  async function scan() {
    setAction("scan");
    setError(null);
    try {
      const result = await invoke<ScanReport>("scan_local_codex", {
        codexHome: codexHome.trim() || null,
      });
      setReport(result);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setAction(null);
    }
  }

  async function createSnapshot() {
    setAction("snapshot");
    setError(null);
    setValidation(null);
    setImportReport(null);
    try {
      const result = await invoke<SnapshotSummary>("create_snapshot", {
        codexHome: codexHome.trim() || null,
        repositoryRoot: repositoryRoot.trim() || null,
        confirmedCodexClosed: confirmedClosed,
      });
      setSnapshot(result);
      setManifestPath(result.manifestPath);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setAction(null);
    }
  }

  async function validateSnapshot() {
    setAction("validate");
    setError(null);
    setImportReport(null);
    try {
      const result = await invoke<SnapshotValidationReport>("validate_snapshot", {
        manifestPath: manifestPath.trim(),
        repositoryRoot: repositoryRoot.trim() || null,
      });
      setValidation(result);
    } catch (reason) {
      setValidation(null);
      setError(String(reason));
    } finally {
      setAction(null);
    }
  }

  async function importSnapshot() {
    setAction("import");
    setError(null);
    try {
      const result = await invoke<ImportReport>("import_snapshot", {
        manifestPath: manifestPath.trim(),
        codexHome: codexHome.trim() || null,
        repositoryRoot: repositoryRoot.trim() || null,
        confirmedCodexClosed: confirmedClosed,
      });
      setImportReport(result);
      setJournalPath(result.journalPath);
      const refreshed = await invoke<ScanReport>("scan_local_codex", {
        codexHome: codexHome.trim() || null,
      });
      setReport(refreshed);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setAction(null);
    }
  }

  async function recoverOperation() {
    setAction("recover");
    setError(null);
    try {
      const result = await invoke<OperationJournal>("recover_operation", {
        journalPath: journalPath.trim(),
        confirmedCodexClosed: confirmedClosed,
      });
      setRecoveredJournal(result);
      const refreshed = await invoke<ScanReport>("scan_local_codex", {
        codexHome: codexHome.trim() || null,
      });
      setReport(refreshed);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setAction(null);
    }
  }

  return (
    <main className="app-shell">
      <header className="hero">
        <div>
          <span className="eyebrow">PHASE 2 · BACKUP & SAFE IMPORT</span>
          <h1>Codex Session Sync</h1>
          <p>创建内容寻址快照，在写入前自动备份，并用操作日志记录可恢复的导入过程。</p>
        </div>
        <div className="status-pill">
          {isTauriRuntime ? "本地数据优先" : "浏览器预览模式"}
        </div>
      </header>

      <section className="panel workspace-panel">
        <div className="field">
          <label htmlFor="codex-home">Codex Home</label>
          <input
            id="codex-home"
            value={codexHome}
            onChange={(event) => setCodexHome(event.target.value)}
            placeholder="~/.codex"
          />
        </div>
        <div className="field">
          <label htmlFor="repository-root">本地同步仓库</label>
          <input
            id="repository-root"
            value={repositoryRoot}
            onChange={(event) => setRepositoryRoot(event.target.value)}
            placeholder="~/.codex-session-sync"
          />
        </div>
        <div className="action-row">
          <button
            className="secondary-button"
            onClick={scan}
            disabled={busy || !codexHome.trim() || !isTauriRuntime}
          >
            {action === "scan" ? "正在扫描…" : "扫描本机会话"}
          </button>
          <button
            onClick={createSnapshot}
            disabled={busy || !confirmedClosed || !isTauriRuntime}
          >
            {action === "snapshot" ? "正在创建…" : "创建本地快照"}
          </button>
        </div>
        <label className="safety-check">
          <input
            type="checkbox"
            checked={confirmedClosed}
            onChange={(event) => setConfirmedClosed(event.target.checked)}
          />
          <span>我已完全退出 Codex；快照和导入期间不会重新启动</span>
        </label>
      </section>

      <section className="panel operation-panel">
        <div className="section-heading">
          <div>
            <h2>快照验证与导入</h2>
            <p>导入前会校验全部对象；同 UUID 内容分叉时不会覆盖。</p>
          </div>
        </div>
        <div className="field">
          <label htmlFor="manifest-path">快照清单路径</label>
          <input
            id="manifest-path"
            value={manifestPath}
            onChange={(event) => setManifestPath(event.target.value)}
            placeholder="~/.codex-session-sync/snapshots/<id>.json"
          />
        </div>
        <div className="action-row compact-actions">
          <button
            className="secondary-button"
            onClick={validateSnapshot}
            disabled={busy || !manifestPath.trim() || !isTauriRuntime}
          >
            {action === "validate" ? "正在验证…" : "验证快照"}
          </button>
          <button
            className="danger-button"
            onClick={importSnapshot}
            disabled={
              busy ||
              !manifestPath.trim() ||
              !confirmedClosed ||
              !isTauriRuntime
            }
          >
            {action === "import" ? "正在安全导入…" : "导入到当前 Codex Home"}
          </button>
        </div>
        <div className="recovery-row">
          <div className="field">
            <label htmlFor="journal-path">未完成操作的 Journal 路径</label>
            <input
              id="journal-path"
              value={journalPath}
              onChange={(event) => setJournalPath(event.target.value)}
              placeholder="~/.codex-session-sync/journal/<operation-id>.json"
            />
          </div>
          <button
            className="recovery-button"
            onClick={recoverOperation}
            disabled={
              busy ||
              !journalPath.trim() ||
              !confirmedClosed ||
              !isTauriRuntime
            }
          >
            {action === "recover" ? "正在恢复…" : "从备份恢复"}
          </button>
        </div>
      </section>

      {error && <div className="error-banner">{error}</div>}

      {(snapshot || validation || importReport || recoveredJournal) && (
        <section className="result-grid">
          {snapshot && (
            <article className="result-card">
              <span>最新快照</span>
              <strong>{snapshot.threadCount} 个会话</strong>
              <small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small>
            </article>
          )}
          {validation && (
            <article className="result-card success-card">
              <span>验证结果</span>
              <strong>{validation.valid ? "完整有效" : "验证失败"}</strong>
              <small>{validation.snapshotId}</small>
            </article>
          )}
          {importReport && (
            <article className="result-card success-card">
              <span>导入完成</span>
              <strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong>
              <small>备份：{importReport.backupDir}</small>
            </article>
          )}
          {recoveredJournal && (
            <article className="result-card success-card">
              <span>恢复结果</span>
              <strong>{recoveredJournal.status}</strong>
              <small>{recoveredJournal.error ?? recoveredJournal.operationId}</small>
            </article>
          )}
        </section>
      )}

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
          <h2>等待扫描</h2>
          <p>先扫描会话；关闭 Codex 后即可创建可验证的本地快照。</p>
        </section>
      )}
    </main>
  );
}
