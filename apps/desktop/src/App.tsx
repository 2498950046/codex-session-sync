import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  CodexProcess,
  ImportReport,
  JobSnapshot,
  OperationJournal,
  ScanReport,
  SnapshotSummary,
  SnapshotValidationReport,
} from "./types";

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / 1024 ** index).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

function isActive(job: JobSnapshot | null): boolean {
  return job?.state === "running" || job?.state === "cancelling";
}

export default function App() {
  const isTauriRuntime = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const [codexHome, setCodexHome] = useState("");
  const [repositoryRoot, setRepositoryRoot] = useState("");
  const [manifestPath, setManifestPath] = useState("");
  const [journalPath, setJournalPath] = useState("");
  const [confirmedClosed, setConfirmedClosed] = useState(false);
  const [processes, setProcesses] = useState<CodexProcess[]>([]);
  const [job, setJob] = useState<JobSnapshot | null>(null);
  const [report, setReport] = useState<ScanReport | null>(null);
  const [snapshot, setSnapshot] = useState<SnapshotSummary | null>(null);
  const [validation, setValidation] = useState<SnapshotValidationReport | null>(null);
  const [importReport, setImportReport] = useState<ImportReport | null>(null);
  const [recoveredJournal, setRecoveredJournal] = useState<OperationJournal | null>(null);
  const [error, setError] = useState<string | null>(null);

  const busy = isActive(job);
  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);
  const progressPercent = job?.progress.total && job.progress.total > 0
    ? Math.min(100, Math.round((job.progress.completed / job.progress.total) * 100))
    : null;

  async function refreshProcesses() {
    if (!isTauriRuntime) return;
    try {
      setProcesses(await invoke<CodexProcess[]>("list_codex_processes"));
    } catch (reason) {
      setError(String(reason));
    }
  }

  useEffect(() => {
    if (!isTauriRuntime) {
      setCodexHome("~/.codex");
      setRepositoryRoot("~/.codex-session-sync");
      return;
    }
    Promise.all([
      invoke<string>("get_default_codex_home"),
      invoke<string>("get_default_repository_root"),
    ]).then(([home, repository]) => {
      setCodexHome(home);
      setRepositoryRoot(repository);
    }).catch((reason) => setError(String(reason)));
    void refreshProcesses();
  }, [isTauriRuntime]);

  useEffect(() => {
    if (!job || !isActive(job)) return;
    const timer = window.setInterval(() => {
      invoke<JobSnapshot>("get_job", { jobId: job.jobId })
        .then((updated) => {
          setJob(updated);
          if (!isActive(updated)) void finishJob(updated);
        })
        .catch((reason) => setError(String(reason)));
    }, 200);
    return () => window.clearInterval(timer);
  }, [job?.jobId, job?.state]);

  async function finishJob(completed: JobSnapshot) {
    if (completed.state === "failed" || completed.state === "cancelled") {
      setError(completed.error ?? "任务未完成");
      return;
    }
    if (!completed.resultReady) {
      setError("任务未提供可领取的结果");
      return;
    }
    try {
      const result = await invoke<unknown>("take_job_result", { jobId: completed.jobId });
      applyJobResult(completed, result);
    } catch (reason) {
      setError(String(reason));
    }
  }

  function applyJobResult(completed: JobSnapshot, result: unknown) {
    if (completed.kind === "scan") setReport(result as ScanReport);
    if (completed.kind === "snapshot") {
      const summary = result as SnapshotSummary;
      setSnapshot(summary);
      setManifestPath(summary.manifestPath);
      setValidation(null);
    }
    if (completed.kind === "validate") setValidation(result as SnapshotValidationReport);
    if (completed.kind === "import") {
      const report = result as ImportReport;
      setImportReport(report);
      setJournalPath(report.journalPath);
    }
    if (completed.kind === "recovery") setRecoveredJournal(result as OperationJournal);
  }

  async function start(command: string, payload: Record<string, unknown>) {
    if (busy) return;
    setError(null);
    try {
      const started = await invoke<JobSnapshot>(command, payload);
      setJob(started);
    } catch (reason) {
      setError(String(reason));
      await refreshProcesses();
    }
  }

  async function cancelCurrentJob() {
    if (!job) return;
    try {
      setJob(await invoke<JobSnapshot>("cancel_job", { jobId: job.jobId }));
    } catch (reason) {
      setError(String(reason));
    }
  }

  function startScan() {
    void start("start_scan_job", { codexHome: codexHome.trim() || null });
  }

  function startSnapshot() {
    void start("start_snapshot_job", {
      codexHome: codexHome.trim() || null,
      repositoryRoot: repositoryRoot.trim() || null,
      confirmedCodexClosed: confirmedClosed,
    });
  }

  function startValidation() {
    void start("start_validation_job", {
      manifestPath: manifestPath.trim(),
      repositoryRoot: repositoryRoot.trim() || null,
    });
  }

  function startImport() {
    void start("start_import_job", {
      manifestPath: manifestPath.trim(),
      codexHome: codexHome.trim() || null,
      repositoryRoot: repositoryRoot.trim() || null,
      confirmedCodexClosed: confirmedClosed,
    });
  }

  function startRecovery() {
    void start("start_recovery_job", {
      journalPath: journalPath.trim(),
      confirmedCodexClosed: confirmedClosed,
    });
  }

  return (
    <main className="app-shell">
      <header className="hero">
        <div>
          <span className="eyebrow">PHASE 2 · SAFE LOCAL SYNC</span>
          <h1>Codex Session Sync</h1>
          <p>扫描、快照和导入都在独立后台任务中执行，可显示真实状态并在安全点中断。</p>
        </div>
        <div className={`status-pill ${processes.length ? "status-warning" : ""}`}>
          {processes.length ? `检测到 ${processes.length} 个 Codex 进程` : "未检测到 Codex 进程"}
        </div>
      </header>

      <section className="process-banner">
        <div>
          <strong>Codex 进程检测</strong>
          <span>{processes.length ? "关闭下列进程后才能创建快照、导入或恢复。" : "可以执行写入型同步操作。"}</span>
        </div>
        <button className="secondary-button" onClick={() => void refreshProcesses()} disabled={busy || !isTauriRuntime}>重新检测</button>
      </section>
      {processes.length > 0 && (
        <div className="process-list">
          {processes.map((process) => <code key={process.pid}>{process.kind} · {process.name} · PID {process.pid}</code>)}
        </div>
      )}

      <section className="panel workspace-panel">
        <div className="field"><label htmlFor="codex-home">Codex Home</label><input id="codex-home" value={codexHome} onChange={(event) => setCodexHome(event.target.value)} /></div>
        <div className="field"><label htmlFor="repository-root">本地同步仓库</label><input id="repository-root" value={repositoryRoot} onChange={(event) => setRepositoryRoot(event.target.value)} /></div>
        <div className="action-row">
          <button className="secondary-button" onClick={startScan} disabled={busy || !codexHome.trim() || !isTauriRuntime}>扫描本机会话</button>
          <button onClick={startSnapshot} disabled={busy || !confirmedClosed || processes.length > 0 || !isTauriRuntime}>创建本地快照</button>
        </div>
        <label className="safety-check"><input type="checkbox" checked={confirmedClosed} onChange={(event) => setConfirmedClosed(event.target.checked)} /><span>我已完全退出 Codex；快照和导入期间不会重新启动</span></label>
      </section>

      <section className="panel operation-panel">
        <div className="section-heading"><div><h2>快照验证与导入</h2><p>导入前校验对象；同 UUID 内容分叉时不会覆盖。</p></div></div>
        <div className="field"><label htmlFor="manifest-path">快照清单路径</label><input id="manifest-path" value={manifestPath} onChange={(event) => setManifestPath(event.target.value)} placeholder="~/.codex-session-sync/snapshots/<id>.json" /></div>
        <div className="action-row compact-actions">
          <button className="secondary-button" onClick={startValidation} disabled={busy || !manifestPath.trim() || !isTauriRuntime}>验证快照</button>
          <button className="danger-button" onClick={startImport} disabled={busy || !manifestPath.trim() || !confirmedClosed || processes.length > 0 || !isTauriRuntime}>导入到当前 Codex Home</button>
        </div>
        <div className="recovery-row">
          <div className="field"><label htmlFor="journal-path">未完成操作的 Journal 路径</label><input id="journal-path" value={journalPath} onChange={(event) => setJournalPath(event.target.value)} placeholder="~/.codex-session-sync/journal/<operation-id>.json" /></div>
          <button className="recovery-button" onClick={startRecovery} disabled={busy || !journalPath.trim() || !confirmedClosed || processes.length > 0 || !isTauriRuntime}>从备份恢复</button>
        </div>
      </section>

      {error && <div className="error-banner">{error}</div>}

      {(snapshot || validation || importReport || recoveredJournal) && <section className="result-grid">
        {snapshot && <article className="result-card"><span>最新快照</span><strong>{snapshot.threadCount} 个会话</strong><small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small></article>}
        {validation && <article className="result-card success-card"><span>验证结果</span><strong>{validation.valid ? "完整有效" : "验证失败"}</strong><small>{validation.snapshotId}</small></article>}
        {importReport && <article className="result-card success-card"><span>导入完成</span><strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong><small>备份：{importReport.backupDir}</small></article>}
        {recoveredJournal && <article className="result-card success-card"><span>恢复结果</span><strong>{recoveredJournal.status}</strong><small>{recoveredJournal.error ?? recoveredJournal.operationId}</small></article>}
      </section>}

      {report ? <>
        <section className="metric-grid">
          <article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article>
          <article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article>
          <article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article>
          <article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article>
        </section>
        <section className="content-grid">
          <article className="panel"><div className="section-heading"><h2>会话预览</h2><span>显示 {report.threads.length} / {report.totalCount}</span></div><div className="thread-list">{recentThreads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title}</strong><span>{thread.workspace.sourcePath ?? "未记录工作目录"}</span></div><small>{thread.modelProvider ?? "unknown"}</small></div>)}</div></article>
          <article className="panel"><div className="section-heading"><h2>兼容性状态</h2><span>{report.databasePaths.length} databases</span></div>{report.warnings.length === 0 ? <p className="success-copy">扫描完成，没有发现需要处理的兼容性问题。</p> : <div className="warning-list">{report.warnings.slice(0, 8).map((warning, index) => <div className="warning-row" key={`${warning.path}-${index}`}><strong>{warning.kind}</strong><span>{warning.message}</span></div>)}</div>}</article>
        </section>
      </> : <section className="empty-state"><div className="empty-icon">↻</div><h2>等待扫描</h2><p>扫描会在后台运行，并显示当前正在处理的会话文件。</p></section>}

      {job && <div className="task-modal-backdrop" role="dialog" aria-modal="true" aria-label="任务进度">
        <section className="task-modal">
          <span className="eyebrow">{job.kind.toUpperCase()} · {job.state.toUpperCase()}</span>
          <h2>{job.progress.phase.replaceAll("_", " ")}</h2>
          <p>{job.progress.message}</p>
          <div className={`progress-track ${progressPercent === null ? "indeterminate" : ""}`}><div className="progress-fill" style={{ width: progressPercent === null ? undefined : `${progressPercent}%` }} /></div>
          <small>{progressPercent === null ? `${job.progress.completed} ${job.progress.unit}` : `${progressPercent}% · ${job.progress.completed}/${job.progress.total} ${job.progress.unit}`}</small>
          {isActive(job) ? <button className="danger-button modal-button" onClick={() => void cancelCurrentJob()} disabled={!job.cancellable || job.state === "cancelling"}>{job.state === "cancelling" ? "正在安全停止…" : job.cancellable ? "取消任务" : "此操作不能取消"}</button> : <button className="secondary-button modal-button" onClick={() => setJob(null)}>关闭</button>}
        </section>
      </div>}
    </main>
  );
}
