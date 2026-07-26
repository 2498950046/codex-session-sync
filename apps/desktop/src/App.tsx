import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  CodexProcess,
  ImportReport,
  JobSnapshot,
  NamespaceMappingState,
  OperationJournal,
  RemoteConnectionStatus,
  RemoteNamespace,
  RemoteNamespaceStatus,
  RemoteProfileSummary,
  ScanReport,
  SnapshotSummary,
  SnapshotValidationReport,
  SyncReport,
  ThreadConflict,
  ThreadConflictVersion,
} from "./types";

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / 1024 ** index).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

function shortHead(head: string | null): string {
  return head ? head.replace("sha256:", "").slice(0, 12) : "空";
}

function isActive(job: JobSnapshot | null): boolean {
  return job?.state === "running" || job?.state === "cancelling";
}

function conflictKindLabel(kind: ThreadConflict["kind"]): string {
  if (kind === "local_deleted_remote_modified") return "本地删除，远端已修改";
  if (kind === "remote_deleted_local_modified") return "本地已修改，远端删除";
  return "本地和远端都已修改";
}

function formatConflictTime(timestamp: number | null): string {
  if (timestamp === null) return "更新时间未知";
  const date = new Date(timestamp);
  return Number.isNaN(date.getTime()) ? "更新时间未知" : date.toLocaleString("zh-CN");
}

function ConflictVersionDetails({ version }: { version: ThreadConflictVersion | null }) {
  if (!version) return <div className="deleted-version"><strong>此版本已删除会话</strong><span>选择它会从合并结果中删除该会话。</span></div>;
  return <>
    <strong className="version-title">{version.title}</strong>
    <span>{formatConflictTime(version.updatedAtMs)} · {version.archived ? "已归档" : "活动"}</span>
    <span>{version.modelProvider ?? "provider 未知"} · {version.workspaceSourcePath ?? "未记录工作目录"}</span>
    <code>{shortHead(version.semanticHash)}</code>
  </>;
}

function selectionSourceLabel(source: NamespaceMappingState["selection"]["source"]): string {
  if (source === "mapping") return "自动映射";
  if (source === "manual_override") return "手动覆盖";
  if (source === "profile_default") return "默认选择";
  if (source === "ambiguous") return "映射冲突";
  return "未选择";
}

function apiKeySourceLabel(source: NamespaceMappingState["context"]["apiKeySource"]): string {
  if (source === "provider_environment") return "provider 环境变量";
  if (source === "auth_json") return "auth.json";
  if (source === "transient_input") return "临时输入";
  return "未检测到";
}

export default function App() {
  const isTauriRuntime = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const [codexHome, setCodexHome] = useState("");
  const [repositoryRoot, setRepositoryRoot] = useState("");
  const [manifestPath, setManifestPath] = useState("");
  const [journalPath, setJournalPath] = useState("");
  const [confirmedClosed, setConfirmedClosed] = useState(false);
  const [confirmedReplaceTarget, setConfirmedReplaceTarget] = useState<string | null>(null);
  const [processes, setProcesses] = useState<CodexProcess[]>([]);
  const [job, setJob] = useState<JobSnapshot | null>(null);
  const [report, setReport] = useState<ScanReport | null>(null);
  const [snapshot, setSnapshot] = useState<SnapshotSummary | null>(null);
  const [validation, setValidation] = useState<SnapshotValidationReport | null>(null);
  const [importReport, setImportReport] = useState<ImportReport | null>(null);
  const [recoveredJournal, setRecoveredJournal] = useState<OperationJournal | null>(null);
  const [syncReport, setSyncReport] = useState<SyncReport | null>(null);
  const [syncReportTargetKey, setSyncReportTargetKey] = useState<string | null>(null);
  const [conflictChoices, setConflictChoices] = useState<Record<string, "local" | "remote">>({});
  const [error, setError] = useState<string | null>(null);
  const jobSyncTargets = useRef(new Map<string, string>());

  const [profiles, setProfiles] = useState<RemoteProfileSummary[]>([]);
  const [selectedRemoteId, setSelectedRemoteId] = useState("");
  const [remoteName, setRemoteName] = useState("个人服务器");
  const [remoteUrl, setRemoteUrl] = useState("http://127.0.0.1:8787");
  const [remoteToken, setRemoteToken] = useState("");
  const [namespaces, setNamespaces] = useState<RemoteNamespace[]>([]);
  const [selectedNamespaceId, setSelectedNamespaceId] = useState("");
  const [namespaceName, setNamespaceName] = useState("");
  const [namespaceStatus, setNamespaceStatus] = useState<RemoteNamespaceStatus | null>(null);
  const [mappingState, setMappingState] = useState<NamespaceMappingState | null>(null);
  const [mappingLabel, setMappingLabel] = useState("");
  const [matchApiKey, setMatchApiKey] = useState(true);
  const [matchProvider, setMatchProvider] = useState(false);
  const [matchCodexHome, setMatchCodexHome] = useState(false);
  const [connectionMessage, setConnectionMessage] = useState<string | null>(null);
  const [remoteLoading, setRemoteLoading] = useState(false);

  const busy = isActive(job) || remoteLoading;
  const canWrite = confirmedClosed && processes.length === 0 && isTauriRuntime;
  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);
  const selectedProfile = profiles.find((profile) => profile.id === selectedRemoteId) ?? null;
  const selectedNamespace = namespaces.find((namespace) => namespace.id === selectedNamespaceId) ?? null;
  const mappingCriteriaValid = Boolean(
    (matchApiKey && mappingState?.context.apiKeyAvailable)
    || (matchProvider && mappingState?.context.provider)
    || matchCodexHome,
  );
  const replaceTargetKey = codexHome.trim() && selectedRemoteId && selectedNamespaceId
    ? JSON.stringify([codexHome.trim(), selectedRemoteId, selectedNamespaceId])
    : null;
  const confirmedReplace = replaceTargetKey !== null
    && confirmedReplaceTarget === replaceTargetKey;
  const syncTargetKey = JSON.stringify([
    repositoryRoot.trim(),
    codexHome.trim(),
    selectedRemoteId,
    selectedNamespaceId,
  ]);
  const activeConflicts = syncReport?.kind === "conflict" && syncReportTargetKey === syncTargetKey
    ? syncReport.conflicts
    : [];
  const resolvedConflictCount = activeConflicts.filter((conflict) => conflictChoices[conflict.conflictId]).length;
  const allConflictsResolved = activeConflicts.length > 0
    && resolvedConflictCount === activeConflicts.length;
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

  async function refreshProfiles(preferredId?: string) {
    if (!isTauriRuntime || !repositoryRoot.trim()) return;
    const loaded = await invoke<RemoteProfileSummary[]>("list_remote_profiles", {
      repositoryRoot: repositoryRoot.trim(),
    });
    setProfiles(loaded);
    const next = preferredId || selectedRemoteId || loaded[0]?.id || "";
    if (next && loaded.some((profile) => profile.id === next)) setSelectedRemoteId(next);
  }

  function applyMappingSelection(state: NamespaceMappingState, available: RemoteNamespace[]) {
    const selected = state.selection.selectedNamespaceId;
    if (selected && available.some((namespace) => namespace.id === selected)) {
      setSelectedNamespaceId(selected);
      return;
    }
    if (selected) {
      setSelectedNamespaceId("");
      setNamespaceStatus(null);
      setError("当前自动选择目标对应的命名空间已不存在，请删除规则或恢复自动选择。");
      return;
    }
    if (state.automaticEnabled) {
      setSelectedNamespaceId("");
      setNamespaceStatus(null);
      return;
    }
    setSelectedNamespaceId(available[0]?.id ?? "");
  }

  async function refreshNamespaces(remoteId = selectedRemoteId) {
    if (!remoteId || !isTauriRuntime || !codexHome.trim()) return;
    setRemoteLoading(true);
    try {
      const [loaded, mappings] = await Promise.all([
        invoke<RemoteNamespace[]>("list_remote_namespaces", {
          repositoryRoot: repositoryRoot.trim(),
          remoteId,
        }),
        invoke<NamespaceMappingState>("get_namespace_mapping_state", {
          repositoryRoot: repositoryRoot.trim(),
          codexHome: codexHome.trim(),
          remoteId,
        }),
      ]);
      setNamespaces(loaded);
      setMappingState(mappings);
      applyMappingSelection(mappings, loaded);
      if (mappings.mappings.length === 0) {
        setMatchApiKey(mappings.context.apiKeyAvailable);
        setMatchCodexHome(!mappings.context.apiKeyAvailable);
      }
      if (!mappings.selection.selectedNamespaceId && mappings.automaticEnabled) {
        setNamespaceStatus(null);
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function refreshMappingState(
    remoteId = selectedRemoteId,
    availableNamespaces = namespaces,
  ) {
    if (!remoteId || !isTauriRuntime || !codexHome.trim()) return null;
    const state = await invoke<NamespaceMappingState>("get_namespace_mapping_state", {
      repositoryRoot: repositoryRoot.trim(),
      codexHome: codexHome.trim(),
      remoteId,
    });
    setMappingState(state);
    applyMappingSelection(state, availableNamespaces);
    return state;
  }

  async function refreshNamespaceStatus(namespaceId = selectedNamespaceId) {
    if (!selectedRemoteId || !namespaceId || !isTauriRuntime) return;
    try {
      setNamespaceStatus(await invoke<RemoteNamespaceStatus>("get_remote_namespace_status", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId,
      }));
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
    if (!repositoryRoot.trim() || !isTauriRuntime) return;
    void refreshProfiles().catch((reason) => setError(String(reason)));
  }, [repositoryRoot, isTauriRuntime]);

  useEffect(() => {
    if (!selectedProfile) return;
    setMappingState(null);
    setRemoteName(selectedProfile.displayName);
    setRemoteUrl(selectedProfile.serverUrl);
    setRemoteToken("");
    setConnectionMessage(null);
    void refreshNamespaces(selectedProfile.id);
  }, [selectedRemoteId]);

  useEffect(() => {
    if (!selectedNamespaceId) return;
    const namespace = namespaces.find((candidate) => candidate.id === selectedNamespaceId);
    setNamespaceName(namespace?.displayName ?? "");
    setMappingLabel((current) => current || `${namespace?.displayName ?? "命名空间"} 自动映射`);
    void refreshNamespaceStatus(selectedNamespaceId);
  }, [selectedNamespaceId, codexHome]);

  useEffect(() => {
    if (!selectedRemoteId || !codexHome.trim() || !isTauriRuntime) return;
    void refreshMappingState().catch((reason) => setError(String(reason)));
  }, [codexHome]);

  useEffect(() => {
    if (!mappingState?.context.apiKeyAvailable) setMatchApiKey(false);
    if (!mappingState?.context.provider) setMatchProvider(false);
  }, [mappingState?.context.apiKeyAvailable, mappingState?.context.provider]);

  useEffect(() => {
    setConfirmedReplaceTarget(null);
  }, [codexHome, selectedRemoteId, selectedNamespaceId]);

  useEffect(() => {
    setConflictChoices({});
  }, [syncTargetKey]);

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
      jobSyncTargets.current.delete(completed.jobId);
      setError(completed.error ?? "任务未完成");
      return;
    }
    if (!completed.resultReady) {
      setError("任务没有可领取的结果");
      return;
    }
    try {
      const result = await invoke<unknown>("take_job_result", { jobId: completed.jobId });
      await applyJobResult(completed, result);
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function applyJobResult(completed: JobSnapshot, result: unknown) {
    if (completed.kind === "scan") setReport(result as ScanReport);
    if (completed.kind === "snapshot") {
      const summary = result as SnapshotSummary;
      setSnapshot(summary);
      setManifestPath(summary.manifestPath);
      setValidation(null);
    }
    if (completed.kind === "validate") setValidation(result as SnapshotValidationReport);
    if (completed.kind === "import") {
      const imported = result as ImportReport;
      setImportReport(imported);
      setJournalPath(imported.journalPath);
    }
    if (completed.kind === "recovery") setRecoveredJournal(result as OperationJournal);
    if (["push", "pull", "resolve", "switch"].includes(completed.kind)) {
      const synced = result as SyncReport;
      setSyncReport(synced);
      setSyncReportTargetKey(jobSyncTargets.current.get(completed.jobId) ?? syncTargetKey);
      jobSyncTargets.current.delete(completed.jobId);
      setConflictChoices({});
      if (synced.checkout) setJournalPath(synced.checkout.journalPath);
      await refreshNamespaces();
      await refreshNamespaceStatus();
      if (completed.kind !== "push") {
        const scanned = await invoke<JobSnapshot>("start_scan_job", { codexHome: codexHome.trim() });
        setJob(scanned);
      }
    }
  }

  async function start(command: string, payload: Record<string, unknown>) {
    if (busy) return;
    if (command === "start_namespace_switch_job") setConfirmedReplaceTarget(null);
    setError(null);
    try {
      const targetKey = syncTargetKey;
      const started = await invoke<JobSnapshot>(command, payload);
      if (["start_push_job", "start_pull_job", "start_conflict_resolution_job", "start_namespace_switch_job"].includes(command)) {
        jobSyncTargets.current.set(started.jobId, targetKey);
      }
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

  async function resolveConflicts() {
    if (!allConflictsResolved) return;
    await start("start_conflict_resolution_job", {
      ...syncPayload,
      resolutions: activeConflicts.map((conflict) => ({
        conflictId: conflict.conflictId,
        threadId: conflict.threadId,
        choice: conflictChoices[conflict.conflictId],
      })),
    });
  }

  async function saveRemote() {
    if (!remoteName.trim() || !remoteUrl.trim()) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const result = await invoke<RemoteConnectionStatus>("save_remote_profile", {
        repositoryRoot: repositoryRoot.trim(),
        remoteId: selectedRemoteId || null,
        displayName: remoteName.trim(),
        serverUrl: remoteUrl.trim(),
        token: remoteToken.trim() || null,
      });
      setConnectionMessage(`连接成功 · 协议 v${result.protocol.protocolVersion} · 服务端 ${result.protocol.version}`);
      setRemoteToken("");
      setNamespaces(result.namespaces);
      await refreshProfiles(result.profile.id);
      setSelectedRemoteId(result.profile.id);
      await refreshMappingState(result.profile.id, result.namespaces);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function testConnection() {
    if (!selectedRemoteId) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const result = await invoke<RemoteConnectionStatus>("test_remote_connection", {
        repositoryRoot: repositoryRoot.trim(),
        remoteId: selectedRemoteId,
      });
      setConnectionMessage(`连接正常 · ${result.namespaces.length} 个命名空间 · 协议 v${result.protocol.protocolVersion}`);
      setNamespaces(result.namespaces);
      await refreshMappingState(selectedRemoteId, result.namespaces);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function createNamespace() {
    if (!selectedRemoteId || !namespaceName.trim()) return;
    setRemoteLoading(true);
    try {
      const created = await invoke<RemoteNamespace>("create_remote_namespace", {
        repositoryRoot: repositoryRoot.trim(),
        remoteId: selectedRemoteId,
        displayName: namespaceName.trim(),
      });
      const available = [...namespaces.filter((namespace) => namespace.id !== created.id), created];
      setNamespaces(available);
      await chooseNamespace(created.id, available);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function renameNamespace() {
    if (!selectedRemoteId || !selectedNamespaceId || !namespaceName.trim()) return;
    setRemoteLoading(true);
    try {
      await invoke("rename_remote_namespace", {
        repositoryRoot: repositoryRoot.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
        displayName: namespaceName.trim(),
      });
      await refreshNamespaces();
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function chooseNamespace(
    namespaceId: string,
    availableNamespaces = namespaces,
  ) {
    if (!selectedRemoteId) return;
    setSelectedNamespaceId(namespaceId);
    try {
      await invoke("select_remote_namespace", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId,
      });
      await refreshProfiles(selectedRemoteId);
      await refreshMappingState(selectedRemoteId, availableNamespaces);
      await refreshNamespaceStatus(namespaceId);
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function setAutomaticSelection(enabled: boolean) {
    if (!selectedRemoteId) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const state = await invoke<NamespaceMappingState>("set_automatic_namespace_selection", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        enabled,
      });
      setMappingState(state);
      applyMappingSelection(state, namespaces);
      await refreshProfiles(selectedRemoteId);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function createMapping() {
    if (!selectedRemoteId || !selectedNamespaceId || !mappingLabel.trim()) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const state = await invoke<NamespaceMappingState>("create_namespace_mapping", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        request: {
          remoteId: selectedRemoteId,
          namespaceId: selectedNamespaceId,
          label: mappingLabel.trim(),
          matchApiKey,
          matchProvider,
          matchCodexHome,
        },
      });
      setMappingState(state);
      applyMappingSelection(state, namespaces);
      setMappingLabel("");
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function deleteMapping(mappingId: string) {
    if (!selectedRemoteId) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const state = await invoke<NamespaceMappingState>("delete_namespace_mapping", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        mappingId,
      });
      setMappingState(state);
      applyMappingSelection(state, namespaces);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function clearManualOverride() {
    if (!selectedRemoteId) return;
    setRemoteLoading(true);
    setError(null);
    try {
      const state = await invoke<NamespaceMappingState>("clear_manual_namespace_override", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
      });
      setMappingState(state);
      applyMappingSelection(state, namespaces);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  const syncPayload = {
    repositoryRoot: repositoryRoot.trim(),
    codexHome: codexHome.trim(),
    remoteId: selectedRemoteId,
    namespaceId: selectedNamespaceId,
    confirmedCodexClosed: confirmedClosed,
  };

  return (
    <main className="app-shell">
      <header className="hero">
        <div>
          <span className="eyebrow">PHASE 6 · NAMESPACE MAPPINGS</span>
          <h1>Codex Session Sync</h1>
          <p>通过自托管服务器在命名空间之间安全推送、拉取和 checkout Codex 会话。</p>
        </div>
        <div className={`status-pill ${processes.length ? "status-warning" : ""}`}>
          {processes.length ? `检测到 ${processes.length} 个 Codex 进程` : "未检测到 Codex 进程"}
        </div>
      </header>

      <section className="process-banner">
        <div><strong>写入安全检查</strong><span>Push、Pull、冲突解决与命名空间切换前必须完全退出 Codex。</span></div>
        <button className="secondary-button" onClick={() => void refreshProcesses()} disabled={busy || !isTauriRuntime}>重新检测</button>
      </section>
      {processes.length > 0 && <div className="process-list">{processes.map((process) => <code key={process.pid}>{process.kind} · {process.name} · PID {process.pid}</code>)}</div>}

      <section className="panel workspace-panel">
        <div className="field"><label htmlFor="codex-home">Codex Home</label><input id="codex-home" value={codexHome} onChange={(event) => setCodexHome(event.target.value)} disabled={busy} /></div>
        <div className="field"><label htmlFor="repository-root">本地同步仓库</label><input id="repository-root" value={repositoryRoot} onChange={(event) => setRepositoryRoot(event.target.value)} disabled={busy} /></div>
        <div className="action-row">
          <button className="secondary-button" onClick={() => void start("start_scan_job", { codexHome: codexHome.trim() })} disabled={busy || !codexHome.trim() || !isTauriRuntime}>扫描本机会话</button>
          <button onClick={() => void start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: confirmedClosed })} disabled={busy || !canWrite}>创建本地快照</button>
        </div>
        <label className="safety-check"><input type="checkbox" checked={confirmedClosed} onChange={(event) => setConfirmedClosed(event.target.checked)} /><span>我已完全退出 Codex；同步期间不会重新启动</span></label>
      </section>

      <section className="panel remote-panel">
        <div className="section-heading"><div><h2>远端服务器</h2><p>Token 仅保存到操作系统凭据库，前端不会读回明文。</p></div><span>{remoteLoading ? "连接中…" : `${profiles.length} 个配置`}</span></div>
        <div className="profile-tabs">
          {profiles.map((profile) => <button key={profile.id} className={selectedRemoteId === profile.id ? "selected" : "secondary-button"} onClick={() => setSelectedRemoteId(profile.id)} disabled={busy}>{profile.displayName}</button>)}
          <button className="secondary-button" onClick={() => { setSelectedRemoteId(""); setRemoteName("个人服务器"); setRemoteUrl("http://127.0.0.1:8787"); setRemoteToken(""); setNamespaces([]); setSelectedNamespaceId(""); setMappingState(null); }} disabled={busy}>＋ 新建远端</button>
        </div>
        <div className="remote-form">
          <div className="field"><label htmlFor="remote-name">配置名称</label><input id="remote-name" value={remoteName} onChange={(event) => setRemoteName(event.target.value)} /></div>
          <div className="field"><label htmlFor="remote-url">服务器 URL</label><input id="remote-url" value={remoteUrl} onChange={(event) => setRemoteUrl(event.target.value)} /></div>
          <div className="field remote-token-field"><label htmlFor="remote-token">Bearer Token</label><input id="remote-token" type="password" value={remoteToken} onChange={(event) => setRemoteToken(event.target.value)} placeholder={selectedProfile?.credentialConfigured ? "已保存在系统凭据库；留空则不修改" : "至少 16 位可见 ASCII 字符"} /></div>
          <div className="action-row compact-actions"><button onClick={() => void saveRemote()} disabled={busy || !remoteName.trim() || !remoteUrl.trim() || (!selectedRemoteId && !remoteToken.trim())}>保存并验证</button><button className="secondary-button" onClick={() => void testConnection()} disabled={busy || !selectedRemoteId}>测试连接</button></div>
        </div>
        {(selectedProfile?.insecureHttp || remoteUrl.trim().startsWith("http://")) && <p className="warning-copy">当前连接未使用 HTTPS。仅建议在本机或可信内网使用。</p>}
        {connectionMessage && <p className="success-copy">{connectionMessage}</p>}
      </section>

      {selectedRemoteId && <section className="panel namespace-panel">
        <div className="section-heading"><div><h2>命名空间</h2><p>活动项对应当前 Codex Home；切换会完整替换本机会话。</p></div><button className="secondary-button" onClick={() => void refreshNamespaces()} disabled={busy}>刷新</button></div>
        <div className="namespace-grid">
          {namespaces.map((namespace) => {
            const selected = namespace.id === selectedNamespaceId;
            const active = namespaceStatus?.active && selected;
            return <button key={namespace.id} className={`namespace-card ${selected ? "selected" : ""}`} onClick={() => void chooseNamespace(namespace.id)} disabled={busy}><span>{namespace.displayName}</span><code>{shortHead(namespace.head)}</code>{active && <small>当前活动</small>}</button>;
          })}
          {namespaces.length === 0 && <p className="muted-copy">服务器上还没有命名空间。</p>}
        </div>
        <div className="namespace-editor">
          <div className="field"><label htmlFor="namespace-name">命名空间名称</label><input id="namespace-name" value={namespaceName} onChange={(event) => setNamespaceName(event.target.value)} /></div>
          <div className="action-row compact-actions"><button onClick={() => void createNamespace()} disabled={busy || !namespaceName.trim()}>创建</button><button className="secondary-button" onClick={() => void renameNamespace()} disabled={busy || !selectedNamespaceId || !namespaceName.trim()}>重命名选中项</button></div>
        </div>

        {mappingState && <div className="mapping-console">
          <div className="mapping-heading"><div><h3>自动命名空间映射</h3><p>规则和 HMAC 指纹只保存在本机；自动匹配仅选择目标，不会自动 checkout。</p></div><label className="toggle-row"><input type="checkbox" checked={mappingState.automaticEnabled} onChange={(event) => void setAutomaticSelection(event.target.checked)} disabled={busy} /><span>{mappingState.automaticEnabled ? "已启用" : "已关闭"}</span></label></div>
          <div className="identity-grid">
            <article><span>当前 Provider</span><strong>{mappingState.context.provider ?? "未检测到"}</strong><small>来自 config.toml</small></article>
            <article><span>API Key 指纹</span><strong>{mappingState.context.apiKeyFingerprintHint ?? "不可用"}</strong><small>{apiKeySourceLabel(mappingState.context.apiKeySource)} · 不返回原始 Key</small></article>
            <article><span>Codex Home</span><strong>{mappingState.context.codexHomeKey}</strong><small>规范化精确匹配</small></article>
          </div>
          <div className={`mapping-resolution ${mappingState.selection.source === "ambiguous" ? "mapping-ambiguous" : ""}`}><div><span>当前选择来源</span><strong>{selectionSourceLabel(mappingState.selection.source)}</strong><small>{mappingState.selection.selectedNamespaceId ? namespaces.find((namespace) => namespace.id === mappingState.selection.selectedNamespaceId)?.displayName ?? mappingState.selection.selectedNamespaceId : mappingState.selection.source === "ambiguous" ? `${mappingState.selection.ambiguousMappingIds.length} 条同优先级规则指向不同命名空间` : "没有可用目标"}</small></div>{mappingState.selection.source === "manual_override" && <button className="secondary-button" onClick={() => void clearManualOverride()} disabled={busy}>恢复自动选择</button>}</div>
          {mappingState.context.warnings.length > 0 && <div className="mapping-warnings">{mappingState.context.warnings.map((warning, index) => <span key={`${warning}-${index}`}>{warning}</span>)}</div>}
          <div className="mapping-builder">
            <div className="field"><label htmlFor="mapping-label">规则名称</label><input id="mapping-label" value={mappingLabel} onChange={(event) => setMappingLabel(event.target.value)} placeholder="例如：工作账号" /></div>
            <div className="mapping-criteria">
              <label><input type="checkbox" checked={matchApiKey} onChange={(event) => setMatchApiKey(event.target.checked)} disabled={busy || !mappingState.context.apiKeyAvailable} /><span>API Key {mappingState.context.apiKeyAvailable ? `· ${mappingState.context.apiKeyFingerprintHint}` : "· 当前不可检测"}</span></label>
              <label><input type="checkbox" checked={matchProvider} onChange={(event) => setMatchProvider(event.target.checked)} disabled={busy || !mappingState.context.provider} /><span>Provider {mappingState.context.provider ? `· ${mappingState.context.provider}` : "· 当前不可检测"}</span></label>
              <label><input type="checkbox" checked={matchCodexHome} onChange={(event) => setMatchCodexHome(event.target.checked)} disabled={busy} /><span>Codex Home</span></label>
            </div>
            <button onClick={() => void createMapping()} disabled={busy || !selectedNamespace || !mappingLabel.trim() || !mappingCriteriaValid}>映射到{selectedNamespace ? `“${selectedNamespace.displayName}”` : "选中的命名空间"}</button>
          </div>
          <div className="mapping-list">{mappingState.mappings.map((mapping) => {
            const target = namespaces.find((namespace) => namespace.id === mapping.namespaceId);
            return <article className="mapping-card" key={mapping.id}><div><strong>{mapping.label}</strong><span>→ {target?.displayName ?? mapping.namespaceId}</span></div><div className="mapping-tags">{mapping.matchesApiKey && <code>KEY {mapping.apiKeyFingerprintHint}</code>}{mapping.provider && <code>PROVIDER {mapping.provider}</code>}{mapping.codexHomeKey && <code>HOME {mapping.codexHomeKey}</code>}</div><button className="danger-button" onClick={() => void deleteMapping(mapping.id)} disabled={busy}>删除</button></article>;
          })}{mappingState.mappings.length === 0 && <p className="muted-copy">还没有本机映射规则。至少选择一个匹配条件后创建。</p>}</div>
        </div>}

        {selectedNamespace && namespaceStatus && <div className="sync-console">
          <div className="sync-status-grid">
            <article><span>选中命名空间</span><strong>{selectedNamespace.displayName}</strong></article>
            <article><span>本机跟踪</span><code>{shortHead(namespaceStatus.integratedHead)}</code></article>
            <article><span>远端 Head</span><code>{shortHead(namespaceStatus.remoteHead)}</code></article>
            <article><span>状态</span><strong>{namespaceStatus.active ? "当前活动" : namespaceStatus.activeNamespaceId ? "需要切换" : "尚未绑定"}</strong></article>
          </div>
          <div className="action-row sync-actions">
            {namespaceStatus.active ? <>
              <button onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite}>推送</button>
              <button className="secondary-button" onClick={() => void start("start_pull_job", syncPayload)} disabled={busy || !canWrite}>拉取</button>
            </> : !namespaceStatus.activeNamespaceId && !namespaceStatus.remoteHead ? <button onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite}>用本机会话初始化并推送</button> : <button className="danger-button" onClick={() => void start("start_namespace_switch_job", { ...syncPayload, confirmedReplaceLocal: confirmedReplace })} disabled={busy || !canWrite || !confirmedReplace}>切换到此命名空间</button>}
          </div>
          {!namespaceStatus.active && <label className="safety-check"><input type="checkbox" checked={confirmedReplace} onChange={(event) => setConfirmedReplaceTarget(event.target.checked ? replaceTargetKey : null)} /><span>我确认切换会先备份，然后用目标命名空间完整替换本机会话</span></label>}
        </div>}
      </section>}

      {error && <div className="error-banner">{error}</div>}

      {syncReport && <section className="panel sync-result">
        <div className="section-heading"><h2>最近同步结果</h2><span>{syncReport.kind}</span></div>
        <div className="result-grid"><article className="result-card success-card"><span>Head</span><strong>{shortHead(syncReport.head)}</strong><small>{syncReport.threadCount} 个会话</small></article><article className="result-card"><span>对象传输</span><strong>↑ {syncReport.uploadedObjects} / ↓ {syncReport.downloadedObjects}</strong><small>{syncReport.checkout ? `备份：${syncReport.checkout.backupDir}` : "无需本地 checkout"}</small></article></div>
        {syncReport.conflicts.length > 0 && activeConflicts.length === 0 && <p className="warning-copy">同步目标已经改变。请切回产生这些冲突的 Codex Home、远端和命名空间，然后重新拉取。</p>}
        {activeConflicts.length > 0 && <div className="conflict-workbench">
          <div className="conflict-summary">
            <div><strong>需要显式解决 {activeConflicts.length} 个同线程冲突</strong><span>每项选择都绑定到基础、本地和远端的内容指纹；内容变化后旧选择会被拒绝。</span></div>
            <span>{resolvedConflictCount} / {activeConflicts.length} 已选择</span>
          </div>
          <div className="conflict-list">{activeConflicts.map((conflict, index) => {
            const choice = conflictChoices[conflict.conflictId];
            return <article className="conflict-item" key={conflict.conflictId}>
              <header><div><span>冲突 {index + 1}</span><h3>{conflict.title}</h3></div><strong>{conflictKindLabel(conflict.kind)}</strong></header>
              <code className="thread-id">{conflict.threadId}</code>
              <div className="conflict-version-grid">
                <div className="version-card base-version"><span className="version-label">共同基础</span><ConflictVersionDetails version={conflict.base} /></div>
                <button type="button" className={`version-card selectable-version ${choice === "local" ? "chosen-version" : ""}`} onClick={() => setConflictChoices((current) => ({ ...current, [conflict.conflictId]: "local" }))} aria-pressed={choice === "local"} disabled={busy}>
                  <span className="version-label">本地版本</span><ConflictVersionDetails version={conflict.local} /><b>{conflict.local ? "保留本地" : "接受本地删除"}</b>
                </button>
                <button type="button" className={`version-card selectable-version ${choice === "remote" ? "chosen-version" : ""}`} onClick={() => setConflictChoices((current) => ({ ...current, [conflict.conflictId]: "remote" }))} aria-pressed={choice === "remote"} disabled={busy}>
                  <span className="version-label">远端版本</span><ConflictVersionDetails version={conflict.remote} /><b>{conflict.remote ? "保留远端" : "接受远端删除"}</b>
                </button>
              </div>
            </article>;
          })}</div>
          <div className="conflict-submit-row"><div><strong>{allConflictsResolved ? "选择已完整，可以安全合并" : `还需选择 ${activeConflicts.length - resolvedConflictCount} 项`}</strong><span>提交会先备份并应用到本机，再以当前远端 Head 做 CAS Push；不会强制覆盖。</span></div><button onClick={() => void resolveConflicts()} disabled={busy || !canWrite || !allConflictsResolved}>应用选择并完成合并</button></div>
        </div>}
      </section>}

      <section className="panel operation-panel">
        <div className="section-heading"><div><h2>本地快照工具</h2><p>保留原有的验证、增量导入和恢复入口，便于诊断与手动操作。</p></div></div>
        <div className="field"><label htmlFor="manifest-path">快照清单路径</label><input id="manifest-path" value={manifestPath} onChange={(event) => setManifestPath(event.target.value)} placeholder="~/.codex-session-sync/snapshots/<id>.json" /></div>
        <div className="action-row compact-actions"><button className="secondary-button" onClick={() => void start("start_validation_job", { manifestPath: manifestPath.trim(), repositoryRoot: repositoryRoot.trim() })} disabled={busy || !manifestPath.trim() || !isTauriRuntime}>验证快照</button><button className="danger-button" onClick={() => void start("start_import_job", { manifestPath: manifestPath.trim(), codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: confirmedClosed })} disabled={busy || !manifestPath.trim() || !canWrite}>增量导入</button></div>
        <div className="recovery-row"><div className="field"><label htmlFor="journal-path">未完成操作的 Journal 路径</label><input id="journal-path" value={journalPath} onChange={(event) => setJournalPath(event.target.value)} /></div><button className="recovery-button" onClick={() => void start("start_recovery_job", { journalPath: journalPath.trim(), confirmedCodexClosed: confirmedClosed })} disabled={busy || !journalPath.trim() || !canWrite}>从备份恢复</button></div>
      </section>

      {(snapshot || validation || importReport || recoveredJournal) && <section className="result-grid">
        {snapshot && <article className="result-card"><span>最新快照</span><strong>{snapshot.threadCount} 个会话</strong><small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small></article>}
        {validation && <article className="result-card success-card"><span>验证结果</span><strong>{validation.valid ? "完整有效" : "验证失败"}</strong><small>{validation.snapshotId}</small></article>}
        {importReport && <article className="result-card success-card"><span>导入完成</span><strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong><small>备份：{importReport.backupDir}</small></article>}
        {recoveredJournal && <article className="result-card success-card"><span>恢复结果</span><strong>{recoveredJournal.status}</strong><small>{recoveredJournal.error ?? recoveredJournal.operationId}</small></article>}
      </section>}

      {report ? <><section className="metric-grid"><article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article><article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article><article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article><article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article></section><section className="content-grid"><article className="panel"><div className="section-heading"><h2>会话预览</h2><span>显示 {report.threads.length} / {report.totalCount}</span></div><div className="thread-list">{recentThreads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title}</strong><span>{thread.workspace.sourcePath ?? "未记录工作目录"}</span></div><small>{thread.modelProvider ?? "unknown"}</small></div>)}</div></article><article className="panel"><div className="section-heading"><h2>兼容性状态</h2><span>{report.databasePaths.length} databases</span></div>{report.warnings.length === 0 ? <p className="success-copy">扫描完成，没有发现阻塞同步的问题。</p> : <div className="warning-list">{report.warnings.slice(0, 8).map((warning, index) => <div className="warning-row" key={`${warning.path}-${index}`}><strong>{warning.kind}</strong><span>{warning.message}</span></div>)}</div>}</article></section></> : <section className="empty-state"><div className="empty-icon">↗</div><h2>等待扫描</h2><p>扫描会在后台运行，不会修改 Codex 数据。</p></section>}

      {job && <div className="task-modal-backdrop" role="dialog" aria-modal="true" aria-label="任务进度"><section className="task-modal"><span className="eyebrow">{job.kind.toUpperCase()} · {job.state.toUpperCase()}</span><h2>{job.progress.phase.replaceAll("_", " ")}</h2><p>{job.progress.message}</p><div className={`progress-track ${progressPercent === null ? "indeterminate" : ""}`}><div className="progress-fill" style={{ width: progressPercent === null ? undefined : `${progressPercent}%` }} /></div><small>{progressPercent === null ? `${job.progress.completed} ${job.progress.unit}` : `${progressPercent}% · ${job.progress.completed}/${job.progress.total} ${job.progress.unit}`}</small>{isActive(job) ? <button className="danger-button modal-button" onClick={() => void cancelCurrentJob()} disabled={!job.cancellable || job.state === "cancelling"}>{job.state === "cancelling" ? "正在安全停止…" : job.cancellable ? "取消任务" : "当前阶段不可取消"}</button> : <button className="secondary-button modal-button" onClick={() => setJob(null)}>关闭</button>}</section></div>}
    </main>
  );
}
