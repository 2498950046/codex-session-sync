import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  AutomaticWorkspaceMappingResult,
  CodexProcess,
  ImportReport,
  JobSnapshot,
  NamespaceMappingState,
  OperationJournal,
  QuarantinedRollout,
  RemoteConnectionStatus,
  RemoteNamespace,
  RemoteNamespaceStatus,
  RemoteProfileSummary,
  ScanReport,
  ScanWarning,
  SnapshotSummary,
  SnapshotValidationReport,
  SyncReport,
  ThreadConflict,
  ThreadConflictVersion,
  WorkspaceCleanupReport,
  WorkspaceCleanupResult,
  WorkspaceMappingState,
  WorkspacePathEntry,
  WorkspacePullPlan,
} from "./types";

type PendingWorkspaceSync = {
  command: "start_pull_job" | "start_namespace_switch_job" | "start_workspace_remap_job";
  payload: Record<string, unknown>;
  plan: WorkspacePullPlan;
};

type WorkspacePathDraft = {
  remotePath: string;
  suggestedSubdirectory: string;
  localPath: string;
};

type WorkspacePathEditorProps = {
  parentDirectory: string;
  drafts: WorkspacePathDraft[];
  busy: boolean;
  submitLabel: string;
  onParentChange: (value: string) => void;
  onChooseParent: () => void;
  onTargetChange: (index: number, value: string) => void;
  onChooseTarget: (index: number) => void;
  onSubmit: () => void;
  onCancel?: () => void;
};

function WorkspacePathEditor({
  parentDirectory,
  drafts,
  busy,
  submitLabel,
  onParentChange,
  onChooseParent,
  onTargetChange,
  onChooseTarget,
  onSubmit,
  onCancel,
}: WorkspacePathEditorProps) {
  const complete = drafts.length > 0 && drafts.every((draft) => draft.localPath.trim());
  return <div className="workspace-path-editor">
    <div className="field workspace-parent-field"><label htmlFor={onCancel ? "sync-workspace-parent" : "migration-workspace-parent"}>统一父目录</label><div className="path-picker-row"><input id={onCancel ? "sync-workspace-parent" : "migration-workspace-parent"} value={parentDirectory} onChange={(event) => onParentChange(event.target.value)} placeholder="输入或选择父目录，自动生成右侧路径" /><button type="button" className="path-picker-button" onClick={onChooseParent} disabled={busy}>选择目录</button></div></div>
    <div className="workspace-path-table">
      <div className="workspace-path-table-head"><span>原路径</span><span>本机目标路径（可逐项修改）</span></div>
      {drafts.map((draft, index) => <div className="workspace-path-table-row" key={draft.remotePath}>
        <code title={draft.remotePath}>{draft.remotePath}</code>
        <div className="path-picker-row"><input value={draft.localPath} onChange={(event) => onTargetChange(index, event.target.value)} placeholder={`目标目录，例如 ${draft.suggestedSubdirectory}`} /><button type="button" className="path-picker-button" onClick={() => onChooseTarget(index)} disabled={busy}>选择</button></div>
      </div>)}
    </div>
    <div className="workspace-editor-actions"><button onClick={onSubmit} disabled={busy || !complete} title={!complete ? "请为每个原路径指定本机目标路径" : undefined}>{submitLabel}</button>{onCancel && <button className="secondary-button" onClick={onCancel} disabled={busy}>取消</button>}</div>
  </div>;
}

function joinWorkspacePath(parent: string, child: string): string {
  const trimmed = parent.trim().replace(/[\\/]+$/, "");
  if (!trimmed) return "";
  const separator = trimmed.includes("\\") && !trimmed.includes("/") ? "\\" : "/";
  return `${trimmed}${separator}${child}`;
}

function buildWorkspaceDrafts(plan: WorkspacePullPlan, parentDirectory: string): WorkspacePathDraft[] {
  const used = new Set<string>();
  return plan.unmappedPaths.map((candidate) => {
    let suffix = 1;
    let child = candidate.suggestedSubdirectory;
    while (used.has(child.toLocaleLowerCase())) {
      suffix += 1;
      child = `${candidate.suggestedSubdirectory}-${suffix}`;
    }
    used.add(child.toLocaleLowerCase());
    return {
      remotePath: candidate.remotePath,
      suggestedSubdirectory: child,
      localPath: joinWorkspacePath(parentDirectory, child),
    };
  });
}

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
  const isDevelopmentPreview = import.meta.env.DEV
    && ["conflict", "mapping"].includes(new URLSearchParams(window.location.search).get("preview") ?? "");
  const isTauriRuntime = (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window)
    || isDevelopmentPreview;
  const [codexHome, setCodexHome] = useState("");
  const [repositoryRoot, setRepositoryRoot] = useState("");
  const [manifestPath, setManifestPath] = useState("");
  const [journalPath, setJournalPath] = useState("");
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
  const [quarantineMessage, setQuarantineMessage] = useState<string | null>(null);
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
  const [workspaceMappingState, setWorkspaceMappingState] = useState<WorkspaceMappingState | null>(null);
  const [workspaceCleanupReport, setWorkspaceCleanupReport] = useState<WorkspaceCleanupReport | null>(null);
  const [workspaceCleanupMessage, setWorkspaceCleanupMessage] = useState<string | null>(null);
  const [pendingWorkspaceSync, setPendingWorkspaceSync] = useState<PendingWorkspaceSync | null>(null);
  const [workspaceSetupMessage, setWorkspaceSetupMessage] = useState<string | null>(null);
  const [workspaceEditorParent, setWorkspaceEditorParent] = useState("");
  const [workspaceDrafts, setWorkspaceDrafts] = useState<WorkspacePathDraft[]>([]);
  const [migrationPlan, setMigrationPlan] = useState<WorkspacePullPlan | null>(null);
  const [migrationParent, setMigrationParent] = useState("");
  const [migrationDrafts, setMigrationDrafts] = useState<WorkspacePathDraft[]>([]);
  const [migrationMessage, setMigrationMessage] = useState<string | null>(null);
  const [remoteWorkspacePrefix, setRemoteWorkspacePrefix] = useState("");
  const [localWorkspacePrefix, setLocalWorkspacePrefix] = useState("");
  const [mappingLabel, setMappingLabel] = useState("");
  const [matchApiKey, setMatchApiKey] = useState(true);
  const [matchProvider, setMatchProvider] = useState(false);
  const [matchCodexHome, setMatchCodexHome] = useState(false);
  const [connectionMessage, setConnectionMessage] = useState<string | null>(null);
  const [remoteLoading, setRemoteLoading] = useState(false);

  const busy = isActive(job) || remoteLoading;
  const canWrite = processes.length === 0 && isTauriRuntime;
  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);
  const workspacePathEntries = useMemo<WorkspacePathEntry[]>(() => {
    if (workspaceCleanupReport) return workspaceCleanupReport.entries;
    return (workspaceMappingState?.mappings ?? []).map((mapping) => ({
      path: mapping.localPrefix,
      activeCount: 0,
      archivedCount: 0,
      mappings: [{ id: mapping.id, remotePrefix: mapping.remotePrefix }],
      codexProjectNames: [],
      directoryState: "unknown",
      cleanupEligible: false,
    }));
  }, [workspaceCleanupReport, workspaceMappingState]);
  const selectedProfile = profiles.find((profile) => profile.id === selectedRemoteId) ?? null;
  const selectedNamespace = namespaces.find((namespace) => namespace.id === selectedNamespaceId) ?? null;
  const writeBlockedReason = busy
    ? "请等待当前任务完成"
    : !isTauriRuntime
      ? "请在 Codex Session Sync 桌面应用中操作"
      : processes.length > 0
        ? "请先完全退出 Codex，然后点击“重新检测”"
        : null;
  const workflowNextStep = remoteLoading
    ? "正在读取远端状态，请稍候。"
    : !codexHome.trim() || !repositoryRoot.trim()
      ? "先确认 Codex Home 和本地同步仓库路径。"
      : profiles.length === 0
        ? "下一步：填写远端服务器信息，然后点击“保存并验证”。"
        : !selectedRemoteId
          ? "下一步：选择一个远端服务器。"
          : namespaces.length === 0
            ? "下一步：创建第一个命名空间。"
            : !selectedNamespaceId
              ? "下一步：选择一个命名空间。"
              : processes.length > 0
                ? "配置和扫描仍可使用；如需同步或修改会话，请完全退出 Codex 后点击“重新检测”。"
                : "准备完成：可以推送、拉取或切换命名空间。拉取时会自动检查项目路径。";
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
  const jobFailure = job?.state === "failed"
    ? job.error ?? "任务失败，但没有返回错误详情"
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
    setWorkspaceMappingState(null);
    setWorkspaceCleanupReport(null);
    setWorkspaceCleanupMessage(null);
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
    void refreshWorkspaceMappings(selectedNamespaceId);
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
    setWorkspaceCleanupReport(null);
    setWorkspaceCleanupMessage(null);
  }, [codexHome, selectedRemoteId, selectedNamespaceId]);

  useEffect(() => {
    setQuarantineMessage(null);
  }, [codexHome, repositoryRoot]);

  useEffect(() => {
    setConflictChoices({});
    setPendingWorkspaceSync(null);
    setWorkspaceSetupMessage(null);
    setWorkspaceEditorParent("");
    setWorkspaceDrafts([]);
    setMigrationPlan(null);
    setMigrationParent("");
    setMigrationDrafts([]);
    setMigrationMessage(null);
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
    if (["push", "pull", "resolve", "switch", "remap"].includes(completed.kind)) {
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

  async function start(command: string, payload: Record<string, unknown>, allowWhilePreparing = false) {
    if (busy && !allowWhilePreparing) return;
    if (command === "start_namespace_switch_job") setConfirmedReplaceTarget(null);
    setError(null);
    try {
      const targetKey = syncTargetKey;
      const started = await invoke<JobSnapshot>(command, payload);
      if (["start_push_job", "start_pull_job", "start_conflict_resolution_job", "start_namespace_switch_job", "start_workspace_remap_job"].includes(command)) {
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

  async function selectJournalFile() {
    if (!isTauriRuntime || busy) return;
    setError(null);
    const repository = repositoryRoot.trim().replace(/[\\/]+$/, "");
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        defaultPath: journalPath.trim() || (repository ? `${repository}/journal` : undefined),
        filters: [{ name: "Checkout Journal", extensions: ["json"] }],
      });
      if (typeof selected === "string") setJournalPath(selected);
    } catch (reason) {
      setError(`无法打开 Journal 文件选择器：${String(reason)}`);
    }
  }

  async function selectLocalWorkspacePrefix() {
    if (!isTauriRuntime || busy) return;
    setError(null);
    try {
      const selected = await open({
        multiple: false,
        directory: true,
        defaultPath: localWorkspacePrefix.trim() || undefined,
      });
      if (typeof selected === "string") setLocalWorkspacePrefix(selected);
    } catch (reason) {
      setError(`无法打开项目目录选择器：${String(reason)}`);
    }
  }

  async function prepareWorkspacePathsAndStart(
    command: "start_pull_job" | "start_namespace_switch_job",
    payload: Record<string, unknown>,
  ) {
    if (busy || !selectedRemoteId || !selectedNamespaceId) return;
    setRemoteLoading(true);
    setError(null);
    setWorkspaceSetupMessage(null);
    let directStart = false;
    try {
      const plan = await invoke<WorkspacePullPlan>("get_workspace_pull_plan", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
      });
      if (plan.unmappedPaths.length === 0) {
        setPendingWorkspaceSync(null);
        directStart = true;
      } else {
        const effectiveCommand = command === "start_pull_job"
          && namespaceStatus?.active
          && namespaceStatus.integratedHead === plan.remoteHead
          ? "start_workspace_remap_job"
          : command;
        setPendingWorkspaceSync({ command: effectiveCommand, payload, plan });
        setWorkspaceEditorParent("");
        setWorkspaceDrafts(buildWorkspaceDrafts(plan, ""));
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
    if (directStart) await start(command, payload, true);
  }

  function changeEditorParent(mode: "sync" | "migration", value: string) {
    const plan = mode === "sync" ? pendingWorkspaceSync?.plan : migrationPlan;
    if (!plan) return;
    if (mode === "sync") {
      setWorkspaceEditorParent(value);
      setWorkspaceDrafts(buildWorkspaceDrafts(plan, value));
    } else {
      setMigrationParent(value);
      setMigrationDrafts(buildWorkspaceDrafts(plan, value));
    }
  }

  async function chooseEditorParent(mode: "sync" | "migration") {
    const plan = mode === "sync" ? pendingWorkspaceSync?.plan : migrationPlan;
    if (!plan || !isTauriRuntime || busy) return;
    setError(null);
    try {
      const selected = await open({
        multiple: false,
        directory: true,
        title: `选择父目录，生成 ${plan.unmappedPaths.length} 个本机项目路径`,
      });
      if (typeof selected === "string") changeEditorParent(mode, selected);
    } catch (reason) {
      setError(`无法打开项目父目录选择器：${String(reason)}`);
    }
  }

  async function chooseEditorTarget(mode: "sync" | "migration", index: number) {
    if (!isTauriRuntime || busy) return;
    const drafts = mode === "sync" ? workspaceDrafts : migrationDrafts;
    const current = drafts[index];
    if (!current) return;
    setError(null);
    try {
      const selected = await open({
        multiple: false,
        directory: true,
        defaultPath: current.localPath.trim() || undefined,
        title: `为 ${current.remotePath} 选择本机目录`,
      });
      if (typeof selected !== "string") return;
      const update = (items: WorkspacePathDraft[]) => items.map((item, itemIndex) => (
        itemIndex === index ? { ...item, localPath: selected } : item
      ));
      if (mode === "sync") setWorkspaceDrafts(update);
      else setMigrationDrafts(update);
    } catch (reason) {
      setError(`无法打开项目目录选择器：${String(reason)}`);
    }
  }

  async function saveWorkspaceDrafts(plan: WorkspacePullPlan, drafts: WorkspacePathDraft[]) {
    return invoke<AutomaticWorkspaceMappingResult>("create_automatic_workspace_mappings", {
      repositoryRoot: repositoryRoot.trim(),
      codexHome: codexHome.trim(),
      request: {
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
        expectedHead: plan.remoteHead,
        mappings: drafts.map((draft) => ({
          remotePath: draft.remotePath,
          localPath: draft.localPath.trim(),
        })),
      },
    });
  }

  async function saveWorkspaceDraftsAndContinue() {
    if (!pendingWorkspaceSync || busy) return;
    setRemoteLoading(true);
    setError(null);
    let shouldContinue = false;
    try {
      const result = await saveWorkspaceDrafts(pendingWorkspaceSync.plan, workspaceDrafts);
      setWorkspaceMappingState(result.state);
      setWorkspaceSetupMessage(
        `已保存 ${workspaceDrafts.length} 条路径规则，创建 ${result.createdDirectories.length} 个目录，正在继续同步。`,
      );
      setPendingWorkspaceSync(null);
      shouldContinue = true;
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
    if (shouldContinue) {
      await start(pendingWorkspaceSync.command, pendingWorkspaceSync.payload, true);
    }
  }

  async function inspectWorkspaceMigration() {
    if (busy || !selectedRemoteId || !selectedNamespaceId) return;
    setRemoteLoading(true);
    setError(null);
    setMigrationMessage(null);
    try {
      const plan = await invoke<WorkspacePullPlan>("get_workspace_pull_plan", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
      });
      setMigrationPlan(plan.unmappedPaths.length > 0 ? plan : null);
      setMigrationParent("");
      setMigrationDrafts(buildWorkspaceDrafts(plan, ""));
      if (plan.unmappedPaths.length === 0) {
        setMigrationMessage("当前命名空间没有需要迁移的未映射项目路径。");
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function saveMigrationDrafts() {
    if (!migrationPlan || busy) return;
    setRemoteLoading(true);
    setError(null);
    let applyToLocal = false;
    try {
      const result = await saveWorkspaceDrafts(migrationPlan, migrationDrafts);
      setWorkspaceMappingState(result.state);
      setMigrationPlan(null);
      setMigrationDrafts([]);
      applyToLocal = Boolean(namespaceStatus?.active && canWrite);
      setMigrationMessage(applyToLocal
        ? `已保存 ${migrationDrafts.length} 条规则，正在安全应用到已有会话。`
        : `已保存 ${migrationDrafts.length} 条规则；下次拉取或切换时会自动应用。`);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
    if (applyToLocal) await start("start_workspace_remap_job", syncPayload, true);
  }

  async function createWorkspaceMapping() {
    if (!selectedRemoteId || !selectedNamespaceId) return;
    setError(null);
    try {
      const state = await invoke<WorkspaceMappingState>("create_workspace_mapping", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        request: {
          remoteId: selectedRemoteId,
          namespaceId: selectedNamespaceId,
          remotePrefix: remoteWorkspacePrefix.trim(),
          localPrefix: localWorkspacePrefix.trim(),
        },
      });
      setWorkspaceMappingState(state);
      setWorkspaceCleanupReport(null);
      setRemoteWorkspacePrefix("");
      setLocalWorkspacePrefix("");
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function deleteWorkspaceMapping(mappingId: string) {
    if (!selectedRemoteId || !selectedNamespaceId) return;
    setError(null);
    try {
      setWorkspaceMappingState(await invoke<WorkspaceMappingState>("delete_workspace_mapping", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
        mappingId,
      }));
      setWorkspaceCleanupReport(null);
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function refreshWorkspaceMappings(namespaceId = selectedNamespaceId) {
    if (!selectedRemoteId || !namespaceId || !codexHome.trim() || !isTauriRuntime) {
      setWorkspaceMappingState(null);
      return;
    }
    try {
      setWorkspaceMappingState(await invoke<WorkspaceMappingState>("get_workspace_mapping_state", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId,
      }));
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function inspectWorkspaceCleanup() {
    if (!selectedRemoteId || !selectedNamespaceId || busy) return;
    setRemoteLoading(true);
    setError(null);
    setWorkspaceCleanupMessage(null);
    try {
      const cleanup = await invoke<WorkspaceCleanupReport>("get_workspace_cleanup_report", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
      });
      setWorkspaceCleanupReport(cleanup);
      if (cleanup.candidates.length === 0) {
        setWorkspaceCleanupMessage(`已聚合 ${cleanup.entries.length} 个项目路径，没有发现可安全清理的空目录或残留 Codex 项目。`);
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setRemoteLoading(false);
    }
  }

  async function cleanupWorkspaceDirectories(paths: string[]) {
    if (!selectedRemoteId || !selectedNamespaceId || busy || !canWrite || paths.length === 0) return;
    const description = paths.length === 1 ? paths[0] : `${paths.length} 个项目路径`;
    if (!window.confirm(`安全清理 ${description}？\n\n操作会重新确认没有路径映射或活动/归档会话引用；空目录会移入可恢复隔离区，同时清除 Codex 左侧菜单中的残留项目记录。`)) return;
    setRemoteLoading(true);
    setError(null);
    setWorkspaceCleanupMessage(null);
    try {
      const result = await invoke<WorkspaceCleanupResult>("quarantine_workspace_directories", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        request: {
          remoteId: selectedRemoteId,
          namespaceId: selectedNamespaceId,
          paths,
        },
        confirmedCodexClosed: true,
      });
      const refreshed = await invoke<WorkspaceCleanupReport>("get_workspace_cleanup_report", {
        repositoryRoot: repositoryRoot.trim(),
        codexHome: codexHome.trim(),
        remoteId: selectedRemoteId,
        namespaceId: selectedNamespaceId,
      });
      setWorkspaceCleanupReport(refreshed);
      const details = [
        result.quarantined.length > 0 ? `隔离 ${result.quarantined.length} 个空目录` : null,
        result.removedCodexProjects > 0 ? `清除 ${result.removedCodexProjects} 个 Codex 项目` : null,
        result.removedThreadAssignments > 0 ? `移除 ${result.removedThreadAssignments} 条残留分配` : null,
      ].filter(Boolean).join("，");
      setWorkspaceCleanupMessage(`清理完成：${details}。备份和操作日志已保存，可在 Codex 完全关闭时恢复。`);
    } catch (reason) {
      setError(String(reason));
      await refreshProcesses();
    } finally {
      setRemoteLoading(false);
    }
  }

  async function quarantineWarning(warning: ScanWarning) {
    if (busy || !canWrite || warning.kind !== "empty_rollout") return;
    if (!window.confirm(`将这个 0 字节 rollout 移入可恢复隔离区？\n\n${warning.path}`)) return;
    setRemoteLoading(true);
    setError(null);
    setQuarantineMessage(null);
    try {
      const result = await invoke<QuarantinedRollout>("quarantine_empty_rollout_file", {
        codexHome: codexHome.trim(),
        repositoryRoot: repositoryRoot.trim(),
        rolloutPath: warning.path,
        confirmedCodexClosed: true,
      });
      setQuarantineMessage(`空文件已移入隔离区：${result.quarantinePath}`);
      const scanned = await invoke<JobSnapshot>("start_scan_job", { codexHome: codexHome.trim() });
      setJob(scanned);
    } catch (reason) {
      setError(String(reason));
      await refreshProcesses();
    } finally {
      setRemoteLoading(false);
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
    confirmedCodexClosed: true,
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
        <div><strong>Codex 运行状态</strong><span>扫描和配置不受影响；仅同步、导入、恢复及清理等一致性操作要求 Codex 完全退出。</span></div>
        <button className="secondary-button" onClick={() => void refreshProcesses()} disabled={busy || !isTauriRuntime}>重新检测</button>
      </section>
      <section className="next-step-banner" aria-live="polite">
        <span>操作引导</span><strong>{workflowNextStep}</strong>
      </section>
      {processes.length > 0 && <div className="process-list">{processes.map((process) => <code key={process.pid}>{process.kind} · {process.name} · PID {process.pid}</code>)}</div>}

      <section className="panel workspace-panel">
        <div className="field"><label htmlFor="codex-home">Codex Home</label><input id="codex-home" value={codexHome} onChange={(event) => setCodexHome(event.target.value)} disabled={busy} /></div>
        <div className="field"><label htmlFor="repository-root">本地同步仓库</label><input id="repository-root" value={repositoryRoot} onChange={(event) => setRepositoryRoot(event.target.value)} disabled={busy} /></div>
        <div className="action-row">
          <button className="secondary-button" onClick={() => void start("start_scan_job", { codexHome: codexHome.trim() })} disabled={busy || !codexHome.trim() || !isTauriRuntime}>扫描本机会话</button>
          <button onClick={() => void start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true })} disabled={busy || !canWrite}>创建本地快照</button>
        </div>
      </section>

      <section className="panel remote-panel">
        <div className="section-heading"><div><h2>远端服务器</h2><p>Token 仅保存到操作系统凭据库，前端不会读回明文。</p></div><span>{remoteLoading ? "连接中…" : `${profiles.length} 个配置`}</span></div>
        <div className="profile-tabs">
          {profiles.map((profile) => <button key={profile.id} className={selectedRemoteId === profile.id ? "selected" : "secondary-button"} onClick={() => setSelectedRemoteId(profile.id)} disabled={busy}>{profile.displayName}</button>)}
          <button className="secondary-button" onClick={() => { setSelectedRemoteId(""); setRemoteName("个人服务器"); setRemoteUrl("http://127.0.0.1:8787"); setRemoteToken(""); setNamespaces([]); setSelectedNamespaceId(""); setMappingState(null); setWorkspaceMappingState(null); }} disabled={busy}>＋ 新建远端</button>
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

        {selectedNamespace && workspaceMappingState && <div className="workspace-mapping-console">
          <div className="mapping-heading"><div><h3>项目路径</h3><p>将路径映射、活动/归档会话、Codex 左侧项目和空目录状态聚合显示；清理时会同时处理磁盘空目录与残留菜单记录。</p></div><div className="workspace-mapping-heading-actions"><span>{workspacePathEntries.length} 个路径</span><button type="button" className="secondary-button" onClick={() => void inspectWorkspaceCleanup()} disabled={busy}>刷新路径状态</button></div></div>
          {workspaceSetupMessage && <p className="success-copy workspace-setup-message">{workspaceSetupMessage}</p>}
          {workspaceCleanupMessage && <p className="success-copy workspace-setup-message">{workspaceCleanupMessage}</p>}
          <div className="workspace-path-summary"><span>映射 {workspacePathEntries.filter((entry) => entry.mappings.length > 0).length}</span><span>Codex 项目 {workspacePathEntries.filter((entry) => entry.codexProjectNames.length > 0).length}</span><span>可清理 {workspaceCleanupReport?.candidates.length ?? 0}</span>{workspaceCleanupReport && workspaceCleanupReport.candidates.length > 0 && <button type="button" className="danger-button" onClick={() => void cleanupWorkspaceDirectories(workspaceCleanupReport.candidates.map((candidate) => candidate.path))} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}>一键安全清理</button>}</div>
          <div className="workspace-path-list">{workspacePathEntries.map((entry) => <article key={entry.path} className={entry.cleanupEligible ? "workspace-path-cleanable" : ""}>
            <div className="workspace-path-main"><code title={entry.path}>{entry.path}</code><div className="workspace-path-badges">{entry.activeCount > 0 && <span className="active-usage">活动 {entry.activeCount}</span>}{entry.archivedCount > 0 && <span className="archived-usage">归档 {entry.archivedCount}</span>}{entry.codexProjectNames.length > 0 && <span className="codex-project-usage">Codex 项目</span>}{entry.directoryState === "empty" && <span className="empty-directory-usage">空目录</span>}{entry.directoryState === "missing" && <span className="missing-directory-usage">目录缺失</span>}{entry.directoryState === "notDirectory" && <span className="blocked-directory-usage">非普通目录</span>}{entry.directoryState === "unknown" && <span className="unknown-directory-usage">待扫描</span>}</div></div>
            {entry.mappings.map((mapping) => <div className="workspace-path-mapping" key={mapping.id}><span><code>{mapping.remotePrefix}</code> → 当前路径</span><button type="button" className="danger-button" onClick={() => void deleteWorkspaceMapping(mapping.id)} disabled={busy}>删除映射</button></div>)}
            {entry.codexProjectNames.length > 0 && <small>Codex 左侧项目：{entry.codexProjectNames.join("、")}</small>}
            {entry.cleanupEligible && <button type="button" className="danger-button workspace-path-cleanup-button" onClick={() => void cleanupWorkspaceDirectories([entry.path])} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}>{entry.directoryState === "missing" ? "清理残留菜单" : entry.codexProjectNames.length > 0 ? "清理空目录和菜单" : "清理空目录"}</button>}
          </article>)}{workspacePathEntries.length === 0 && <p className="muted-copy">没有项目路径记录。刷新后会聚合显示本机映射、会话路径和 Codex 左侧项目。</p>}</div>
          <details className="advanced-mapping">
            <summary>高级：手动添加根路径规则</summary>
            <div className="workspace-mapping-builder">
              <div className="field"><label htmlFor="remote-workspace-prefix">源电脑项目根路径</label><input id="remote-workspace-prefix" value={remoteWorkspacePrefix} onChange={(event) => setRemoteWorkspacePrefix(event.target.value)} placeholder="例如 D:\projects" /></div>
              <div className="field"><label htmlFor="local-workspace-prefix">当前电脑项目根路径</label><div className="path-picker-row"><input id="local-workspace-prefix" value={localWorkspacePrefix} onChange={(event) => setLocalWorkspacePrefix(event.target.value)} placeholder="例如 F:\workspace" /><button type="button" className="path-picker-button" onClick={() => void selectLocalWorkspacePrefix()} disabled={busy || !isTauriRuntime}>选择目录</button></div></div>
              <button onClick={() => void createWorkspaceMapping()} disabled={busy || !remoteWorkspacePrefix.trim() || !localWorkspacePrefix.trim()} title={!remoteWorkspacePrefix.trim() || !localWorkspacePrefix.trim() ? "请填写源路径和本机路径" : undefined}>添加路径映射</button>
            </div>
          </details>
          {workspaceMappingState.mappings.length > 0 && <div className="workspace-remap-row"><p>如需让已经拉取到本机的会话立即使用新规则，可安全备份并重新整理。</p><button className="secondary-button" onClick={() => void start("start_workspace_remap_job", syncPayload)} disabled={busy || !canWrite || !namespaceStatus?.active} title={!namespaceStatus?.active ? "只能整理当前活动命名空间" : writeBlockedReason ?? undefined}>应用到已有会话</button></div>}
        </div>}

        {selectedNamespace && namespaceStatus && <div className="sync-console">
          <div className="sync-status-grid">
            <article><span>选中命名空间</span><strong>{selectedNamespace.displayName}</strong></article>
            <article><span>本机跟踪</span><code>{shortHead(namespaceStatus.integratedHead)}</code></article>
            <article><span>远端 Head</span><code>{shortHead(namespaceStatus.remoteHead)}</code></article>
            <article><span>状态</span><strong>{namespaceStatus.active ? "当前活动" : namespaceStatus.activeNamespaceId ? "需要切换" : "尚未绑定"}</strong></article>
          </div>
          <div className={`sync-guidance ${writeBlockedReason ? "sync-guidance-blocked" : ""}`}><strong>{writeBlockedReason ? "操作尚未就绪" : "可以开始同步"}</strong><span>{writeBlockedReason ?? "拉取会先检查远端项目路径；需要调整时会让你一次选择父目录。"}</span></div>
          <div className="action-row sync-actions">
            {namespaceStatus.active ? <>
              <button onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}>推送</button>
              <button className="secondary-button" onClick={() => void prepareWorkspacePathsAndStart("start_pull_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}>拉取</button>
            </> : !namespaceStatus.activeNamespaceId && !namespaceStatus.remoteHead ? <button onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}>用本机会话初始化并推送</button> : <button className="danger-button" onClick={() => void prepareWorkspacePathsAndStart("start_namespace_switch_job", { ...syncPayload, confirmedReplaceLocal: confirmedReplace })} disabled={busy || !canWrite || !confirmedReplace} title={!confirmedReplace ? "请先勾选下方的替换确认" : writeBlockedReason ?? undefined}>切换到此命名空间</button>}
          </div>
          {!namespaceStatus.active && <label className="safety-check"><input type="checkbox" checked={confirmedReplace} onChange={(event) => setConfirmedReplaceTarget(event.target.checked ? replaceTargetKey : null)} /><span>我确认切换会先备份，然后用目标命名空间完整替换本机会话</span></label>}
        </div>}
      </section>}

      {selectedNamespace && workspaceMappingState && <section className="panel migration-panel">
        <div className="section-heading"><div><h2>会话项目路径迁移</h2><p>复用同步前的路径分析，批量把尚未适配当前电脑的项目迁移到新目录；已存在和已映射路径不会改动。</p></div><button className="secondary-button" onClick={() => void inspectWorkspaceMigration()} disabled={busy} title={busy ? "请等待当前任务完成" : undefined}>检查项目路径</button></div>
        {migrationMessage && <p className="success-copy">{migrationMessage}</p>}
        {migrationPlan && <>
          <div className="migration-summary"><strong>待迁移 {migrationPlan.unmappedPaths.length} 项</strong><span>{migrationPlan.mappedPathCount} 项已有映射 · {migrationPlan.existingPathCount} 项原路径可用</span></div>
          <WorkspacePathEditor
            parentDirectory={migrationParent}
            drafts={migrationDrafts}
            busy={busy}
            submitLabel={namespaceStatus?.active && canWrite ? "保存并应用到已有会话" : "保存路径规则"}
            onParentChange={(value) => changeEditorParent("migration", value)}
            onChooseParent={() => void chooseEditorParent("migration")}
            onTargetChange={(index, value) => setMigrationDrafts((current) => current.map((draft, draftIndex) => draftIndex === index ? { ...draft, localPath: value } : draft))}
            onChooseTarget={(index) => void chooseEditorTarget("migration", index)}
            onSubmit={() => void saveMigrationDrafts()}
          />
        </>}
      </section>}

      {error && <div className="error-banner">{error}</div>}

      {pendingWorkspaceSync && <div className="task-modal-backdrop" role="dialog" aria-modal="true" aria-label="设置本机项目路径">
        <section className="workspace-path-modal">
          <div className="workspace-modal-heading"><div><span className="eyebrow">同步前路径检查</span><h2>设置本机项目路径</h2></div><button type="button" className="modal-close-button" onClick={() => setPendingWorkspaceSync(null)} disabled={busy} aria-label="关闭">×</button></div>
          <p>发现 {pendingWorkspaceSync.plan.unmappedPaths.length} 个本机不存在且尚未映射的项目路径。选择一个父目录可批量生成目标；右侧每一项仍可单独输入或选择。</p>
          <div className="migration-summary"><strong>{pendingWorkspaceSync.plan.unmappedPaths.length} 项待设置</strong><span>{pendingWorkspaceSync.plan.mappedPathCount} 项已有映射 · {pendingWorkspaceSync.plan.existingPathCount} 项原路径可用，不会改动</span></div>
          <WorkspacePathEditor
            parentDirectory={workspaceEditorParent}
            drafts={workspaceDrafts}
            busy={busy}
            submitLabel="创建目录并继续同步"
            onParentChange={(value) => changeEditorParent("sync", value)}
            onChooseParent={() => void chooseEditorParent("sync")}
            onTargetChange={(index, value) => setWorkspaceDrafts((current) => current.map((draft, draftIndex) => draftIndex === index ? { ...draft, localPath: value } : draft))}
            onChooseTarget={(index) => void chooseEditorTarget("sync", index)}
            onSubmit={() => void saveWorkspaceDraftsAndContinue()}
            onCancel={() => setPendingWorkspaceSync(null)}
          />
        </section>
      </div>}

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
        <div className="action-row compact-actions"><button className="secondary-button" onClick={() => void start("start_validation_job", { manifestPath: manifestPath.trim(), repositoryRoot: repositoryRoot.trim() })} disabled={busy || !manifestPath.trim() || !isTauriRuntime}>验证快照</button><button className="danger-button" onClick={() => void start("start_import_job", { manifestPath: manifestPath.trim(), codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true })} disabled={busy || !manifestPath.trim() || !canWrite}>增量导入</button></div>
        <div className="recovery-row"><div className="field"><label htmlFor="journal-path">未完成操作的 Journal 路径</label><div className="path-picker-row"><input id="journal-path" value={journalPath} onChange={(event) => setJournalPath(event.target.value)} placeholder="选择 checkout-*.json" /><button type="button" className="path-picker-button" onClick={() => void selectJournalFile()} disabled={busy || !isTauriRuntime}>选择文件</button></div></div><button className="recovery-button" onClick={() => void start("start_recovery_job", { journalPath: journalPath.trim(), confirmedCodexClosed: true })} disabled={busy || !journalPath.trim() || !canWrite}>从备份恢复</button></div>
      </section>

      {(snapshot || validation || importReport || recoveredJournal) && <section className="result-grid">
        {snapshot && <article className="result-card"><span>最新快照</span><strong>{snapshot.threadCount} 个会话</strong><small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small></article>}
        {validation && <article className="result-card success-card"><span>验证结果</span><strong>{validation.valid ? "完整有效" : "验证失败"}</strong><small>{validation.snapshotId}</small></article>}
        {importReport && <article className="result-card success-card"><span>导入完成</span><strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong><small>备份：{importReport.backupDir}</small></article>}
        {recoveredJournal && <article className="result-card success-card"><span>恢复结果</span><strong>{recoveredJournal.status}</strong><small>{recoveredJournal.error ?? recoveredJournal.operationId}</small></article>}
      </section>}

      {report ? <><section className="metric-grid"><article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article><article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article><article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article><article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article></section><section className="content-grid"><article className="panel"><div className="section-heading"><h2>会话预览</h2><span>显示 {report.threads.length} / {report.totalCount}</span></div><div className="thread-list">{recentThreads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title}</strong><span>{thread.workspace.sourcePath ?? "未记录工作目录"}</span></div><small>{thread.modelProvider ?? "unknown"}</small></div>)}</div></article><article className="panel"><div className="section-heading"><h2>兼容性状态</h2><span>{report.databasePaths.length} databases</span></div>{quarantineMessage && <p className="success-copy">{quarantineMessage}</p>}{report.warnings.length === 0 ? <p className="success-copy">扫描完成，没有发现阻塞同步的问题。</p> : <div className="warning-list">{report.warnings.slice(0, 8).map((warning, index) => <div className="warning-row" key={`${warning.path}-${index}`}><strong>{warning.kind}</strong><span>{warning.message}</span><code>{warning.path}</code>{warning.kind === "empty_rollout" && <button type="button" className="warning-cleanup-button" onClick={() => void quarantineWarning(warning)} disabled={busy || !canWrite}>清理空文件</button>}</div>)}</div>}</article></section></> : <section className="empty-state"><div className="empty-icon">↗</div><h2>等待扫描</h2><p>扫描会在后台运行，不会修改 Codex 数据。</p></section>}

      {job && <div className="task-modal-backdrop" role="dialog" aria-modal="true" aria-label="任务进度"><section className={`task-modal ${jobFailure ? "task-modal-failed" : ""}`}><span className="eyebrow">{job.kind.toUpperCase()} · {job.state.toUpperCase()}</span><h2>{jobFailure ? "任务失败" : job.progress.phase.replaceAll("_", " ")}</h2><p className={jobFailure ? "task-modal-error" : undefined}>{jobFailure ?? job.progress.message}</p><div className={`progress-track ${progressPercent === null ? "indeterminate" : ""}`}><div className="progress-fill" style={{ width: progressPercent === null ? undefined : `${progressPercent}%` }} /></div><small>{progressPercent === null ? `${job.progress.completed} ${job.progress.unit}` : `${progressPercent}% · ${job.progress.completed}/${job.progress.total} ${job.progress.unit}`}</small>{isActive(job) ? <button className="danger-button modal-button" onClick={() => void cancelCurrentJob()} disabled={!job.cancellable || job.state === "cancelling"}>{job.state === "cancelling" ? "正在安全停止…" : job.cancellable ? "取消任务" : "当前阶段不可取消"}</button> : <button className="secondary-button modal-button" onClick={() => setJob(null)}>关闭</button>}</section></div>}
    </main>
  );
}
