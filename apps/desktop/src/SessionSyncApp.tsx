import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { Navigate, NavLink, Route, Routes, useLocation, useNavigate } from "./router";
import {
  AlertTriangle,
  ArchiveRestore,
  ArrowDownToLine,
  ArrowRight,
  ArrowUpFromLine,
  Check,
  ChevronRight,
  CircleHelp,
  Copy,
  Database,
  FolderCog,
  GitBranch,
  KeyRound,
  Moon,
  Plus,
  RefreshCw,
  RotateCcw,
  Server,
  Settings,
  ShieldAlert,
  Sun,
  Trash2,
  X,
} from "lucide-react";
import { AppShell } from "./AppShell";
import { useTheme } from "./theme";
import type {
  AutomaticWorkspaceMappingResult,
  CheckoutReport,
  CodexProcess,
  ImportReport,
  GcPlan,
  JobSnapshot,
  LocalSnapshotListItem,
  NamespaceMappingState,
  OperationJournal,
  ProviderSyncPreview,
  ProviderSyncReport,
  QuarantinedRollout,
  RepositoryStorageSummary,
  RecoveryPoint,
  RemoteConnectionStatus,
  RemoteNamespace,
  RemoteNamespaceStatus,
  RemoteProfileSummary,
  RemoteHistoryTrashOperation,
  RevisionSummary,
  ScanReport,
  ScanWarning,
  SnapshotSummary,
  SnapshotDeletionPlan,
  SnapshotTrashEntry,
  SnapshotValidationReport,
  SyncReport,
  ThreadConflict,
  ThreadConflictVersion,
  WorkspaceCleanupReport,
  WorkspaceCleanupResult,
  WorkspaceMappingState,
  WorkspacePathFilter,
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

export type AppRoute = "/overview" | "/sync" | "/history" | "/sessions" | "/namespaces" | "/settings" | "/advanced";

type ConfirmationRequest = {
  title: string;
  description: ReactNode;
  confirmLabel: string;
  tone?: "warning" | "danger";
  onConfirm: () => void | Promise<void>;
};

function PageIntro({ title, description, action }: { title: string; description: string; action?: ReactNode }) {
  return <div className="page-intro"><div><h2>{title}</h2><p>{description}</p></div>{action}</div>;
}

function StatusBadge({ tone = "neutral", children }: { tone?: "neutral" | "success" | "warning" | "danger"; children: ReactNode }) {
  return <span className={`status-badge ${tone}`}>{children}</span>;
}

type VersionRow = {
  id: string;
  title: string;
  createdAt: string;
  threadCount: number;
  logicalBytes: number;
  labels: string[];
  kind: "local" | "remote";
};

function VersionGraphTable({ rows, selectedId, onSelect }: {
  rows: VersionRow[];
  selectedId: string | null;
  onSelect: (row: VersionRow) => void;
}) {
  return <div className="version-log" role="table" aria-label="版本历史">
    <div className="version-log-head" role="row"><span>Graph</span><span>说明</span><span>标签</span><span>创建时间</span><span>会话</span><span>逻辑大小</span></div>
    {rows.map((row, index) => <button type="button" role="row" key={`${row.kind}-${row.id}`} className={`version-log-row ${selectedId === row.id ? "selected" : ""}`} onClick={() => onSelect(row)}>
      <span className="graph-cell"><i className={`graph-line ${index === rows.length - 1 ? "last" : ""}`} /><i className={`graph-node ${row.kind}`} /></span>
      <span className="version-description"><strong>{row.title}</strong><code>{shortHead(row.id)}</code></span>
      <span className="version-labels">{row.labels.map((label) => <b key={label}>{label}</b>)}</span>
      <span>{new Date(row.createdAt).toLocaleString("zh-CN")}</span>
      <span>{row.threadCount}</span>
      <span>{formatBytes(row.logicalBytes)}</span>
    </button>)}
    {rows.length === 0 && <div className="version-log-empty">暂无版本记录</div>}
  </div>;
}

function CopyCode({ value, compact = false }: { value: string; compact?: boolean }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      // Clipboard access is optional in WebView contexts; the full value remains available via title.
    }
  }
  return <button type="button" className={`copy-code ${compact ? "compact" : ""}`} onClick={() => void copy()} title={value}>
    <code>{compact ? shortHead(value) : value}</code>{copied ? <Check size={13} /> : <Copy size={13} />}
  </button>;
}

function ConfirmDialog({ request, onClose }: { request: ConfirmationRequest | null; onClose: () => void }) {
  const cancelRef = useRef<HTMLButtonElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!request) return;
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    cancelRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
      if (event.key === "Tab") {
        const buttons = Array.from(document.querySelectorAll<HTMLButtonElement>(".confirm-dialog button:not(:disabled)"));
        if (buttons.length < 2) return;
        const first = buttons[0];
        const last = buttons[buttons.length - 1];
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      previousFocus.current?.focus();
    };
  }, [request, onClose]);

  if (!request) return null;
  return <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <section className="confirm-dialog" role="dialog" aria-modal="true" aria-labelledby="confirm-dialog-title">
      <div className={`dialog-icon ${request.tone ?? "warning"}`}><ShieldAlert size={22} /></div>
      <div className="dialog-copy"><h2 id="confirm-dialog-title">{request.title}</h2><div>{request.description}</div></div>
      <div className="dialog-actions">
        <button ref={cancelRef} type="button" className="button secondary" onClick={onClose}>取消</button>
        <button type="button" className={`button ${request.tone === "danger" ? "danger" : "warning"}`} onClick={() => {
          const action = request.onConfirm;
          onClose();
          void action();
        }}>{request.confirmLabel}</button>
      </div>
    </section>
  </div>;
}

function ErrorDialog({ message, onClose }: { message: string | null; onClose: () => void }) {
  const closeRef = useRef<HTMLButtonElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!message) return;
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    closeRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
      if (event.key === "Tab") {
        event.preventDefault();
        closeRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      previousFocus.current?.focus();
    };
  }, [message]);

  if (!message) return null;
  return <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
    <section className="error-dialog" role="alertdialog" aria-modal="true" aria-labelledby="error-dialog-title" aria-describedby="error-dialog-message">
      <div className="dialog-icon danger"><AlertTriangle size={22} /></div>
      <div className="dialog-copy"><h2 id="error-dialog-title">操作未完成</h2><p id="error-dialog-message">{message}</p></div>
      <div className="dialog-actions"><button ref={closeRef} type="button" className="button primary" onClick={onClose}>知道了</button></div>
    </section>
  </div>;
}

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

export default function SessionSyncApp() {
  const navigate = useNavigate();
  const location = useLocation();
  const { preference: themePreference, resolvedTheme, setPreference: setThemePreference } = useTheme();
  const isDevelopmentPreview = import.meta.env.DEV
    && ["ready", "empty", "process-running", "job", "mapping", "conflict", "failure"].includes(new URLSearchParams(window.location.search).get("preview") ?? "");
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
  const [localSnapshots, setLocalSnapshots] = useState<LocalSnapshotListItem[]>([]);
  const [remoteRevisions, setRemoteRevisions] = useState<RevisionSummary[]>([]);
  const [snapshotTrash, setSnapshotTrash] = useState<SnapshotTrashEntry[]>([]);
  const [remoteHistoryTrash, setRemoteHistoryTrash] = useState<RemoteHistoryTrashOperation[]>([]);
  const [selectedHistoryId, setSelectedHistoryId] = useState<string | null>(null);
  const [historySource, setHistorySource] = useState<"local" | "remote" | "recovery" | "trash">("local");
  const [gcPlan, setGcPlan] = useState<GcPlan | null>(null);
  const [storageSummary, setStorageSummary] = useState<RepositoryStorageSummary | null>(null);
  const [recoveryPoints, setRecoveryPoints] = useState<RecoveryPoint[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [validation, setValidation] = useState<SnapshotValidationReport | null>(null);
  const [importReport, setImportReport] = useState<ImportReport | null>(null);
  const [providerSyncPreview, setProviderSyncPreview] = useState<ProviderSyncPreview | null>(null);
  const [providerPreviewLoading, setProviderPreviewLoading] = useState(false);
  const [providerSyncReport, setProviderSyncReport] = useState<ProviderSyncReport | null>(null);
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
  const [workspacePathFilter, setWorkspacePathFilter] = useState<WorkspacePathFilter>("all");
  const [workspacePathQuery, setWorkspacePathQuery] = useState("");
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
  const [confirmation, setConfirmation] = useState<ConfirmationRequest | null>(null);
  const providerPreviewInFlight = useRef(false);

  const busy = isActive(job) || remoteLoading || providerPreviewLoading;
  const providerPreviewActive = providerPreviewLoading
    || (job?.kind === "provider_sync_preview" && isActive(job));
  const canWrite = processes.length === 0 && isTauriRuntime;
  const recentThreads = useMemo(() => report?.threads.slice(0, 8) ?? [], [report]);
  const allWorkspacePathEntries = useMemo<WorkspacePathEntry[]>(() => {
    if (workspaceCleanupReport) return workspaceCleanupReport.entries;
    return (workspaceMappingState?.mappings ?? []).map((mapping) => ({
      path: mapping.localPrefix,
      activeCount: 0,
      archivedCount: 0,
      mappings: [{ id: mapping.id, remotePrefix: mapping.remotePrefix, localPrefix: mapping.localPrefix, inherited: false }],
      codexProjectNames: [],
      directoryState: "unknown",
      cleanupEligible: false,
    }));
  }, [workspaceCleanupReport, workspaceMappingState]);
  const workspacePathEntries = useMemo<WorkspacePathEntry[]>(() => {
    const query = workspacePathQuery.trim().toLocaleLowerCase();
    return allWorkspacePathEntries.filter((entry) => {
      const matchesFilter = workspacePathFilter === "all"
        || (workspacePathFilter === "active" && entry.activeCount > 0)
        || (workspacePathFilter === "archived" && entry.archivedCount > 0)
        || (workspacePathFilter === "codexProject" && entry.codexProjectNames.length > 0)
        || (workspacePathFilter === "mapped" && entry.mappings.length > 0)
        || (workspacePathFilter === "cleanup" && entry.cleanupEligible);
      if (!matchesFilter) return false;
      if (!query) return true;
      return [
        entry.path,
        ...entry.codexProjectNames,
        ...entry.mappings.flatMap((mapping) => [mapping.remotePrefix, mapping.localPrefix]),
      ].some((value) => value.toLocaleLowerCase().includes(query));
    });
  }, [allWorkspacePathEntries, workspacePathFilter, workspacePathQuery]);
  const workspacePathFilterPanel = (
    <div className="workspace-path-filter" aria-label="项目路径筛选">
      <label htmlFor="workspace-path-filter-select">状态</label>
      <select
        id="workspace-path-filter-select"
        value={workspacePathFilter}
        onChange={(event) => setWorkspacePathFilter(event.target.value as WorkspacePathFilter)}
      >
        <option value="all">全部路径</option>
        <option value="active">有活动会话</option>
        <option value="archived">有归档会话</option>
        <option value="codexProject">Codex 项目</option>
        <option value="mapped">已有路径映射</option>
        <option value="cleanup">可安全清理</option>
      </select>
      <label htmlFor="workspace-path-query">搜索</label>
      <input
        id="workspace-path-query"
        type="search"
        value={workspacePathQuery}
        onChange={(event) => setWorkspacePathQuery(event.target.value)}
        placeholder="路径或项目名"
      />
      <span className="workspace-path-filter-count">显示 {workspacePathEntries.length} / {allWorkspacePathEntries.length}</span>
      <small>活动来自会话 cwd；Codex 项目来自 Codex 菜单记录，同一路径可以同时存在。</small>
    </div>
  );
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
  const progressDetail = job
    ? job.progress.unit === "bytes"
      ? progressPercent === null
        ? formatBytes(job.progress.completed)
        : `${progressPercent}% · ${formatBytes(job.progress.completed)}/${formatBytes(job.progress.total ?? 0)}`
      : progressPercent === null
        ? `${job.progress.completed} ${job.progress.unit}`
        : `${progressPercent}% · ${job.progress.completed}/${job.progress.total} ${job.progress.unit}`
    : "";
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
    setProviderSyncPreview(null);
    setProviderSyncReport(null);
  }, [codexHome, repositoryRoot]);

  useEffect(() => {
    if (location.pathname !== "/history" && location.pathname !== "/sync") return;
    void refreshHistory();
  }, [location.pathname, repositoryRoot, selectedRemoteId, selectedNamespaceId]);

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
      await refreshHistory();
    }
    if (completed.kind === "validate") setValidation(result as SnapshotValidationReport);
    if (completed.kind === "provider_sync_preview") setProviderSyncPreview(result as ProviderSyncPreview);
    if (completed.kind === "import") {
      const imported = result as ImportReport;
      setImportReport(imported);
      setJournalPath(imported.journalPath);
    }
    if (completed.kind === "provider_sync") {
      const synced = result as ProviderSyncReport;
      setProviderSyncReport(synced);
      setJournalPath(synced.journalPath);
      setProviderSyncPreview(null);
      const scanned = await invoke<JobSnapshot>("start_scan_job", { codexHome: codexHome.trim() });
      setJob(scanned);
    }
    if (completed.kind === "recovery") setRecoveredJournal(result as OperationJournal);
    if (completed.kind === "restore") {
      const restored = result as CheckoutReport;
      setJournalPath(restored.journalPath);
      await refreshHistory();
    }
    if (completed.kind === "revision-download") {
      const downloaded = result as SnapshotSummary;
      setSnapshot(downloaded);
      setManifestPath(downloaded.manifestPath);
      await refreshHistory();
    }
    if (completed.kind === "revision-restore") {
      const restored = result as CheckoutReport;
      setJournalPath(restored.journalPath);
    }
    if (completed.kind === "revision-publish") {
      const published = result as SyncReport;
      setSyncReport(published);
      await refreshNamespaces();
      await refreshHistory();
    }
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
    if ((busy || providerPreviewInFlight.current) && !allowWhilePreparing) return;
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

  async function previewProviderSync() {
    if (!isTauriRuntime || busy || providerPreviewInFlight.current) return;
    providerPreviewInFlight.current = true;
    setProviderPreviewLoading(true);
    setProviderSyncPreview(null);
    setError(null);
    try {
      setJob(await invoke<JobSnapshot>("start_provider_sync_preview_job", {
        codexHome: codexHome.trim(),
        repositoryRoot: repositoryRoot.trim(),
      }));
    } catch (reason) {
      setError(String(reason));
    } finally {
      providerPreviewInFlight.current = false;
      setProviderPreviewLoading(false);
    }
  }

  async function refreshHistory() {
    if (!isTauriRuntime || !repositoryRoot.trim()) return;
    setHistoryLoading(true);
    try {
      const [local, trash, storage, recovery] = await Promise.all([
        invoke<LocalSnapshotListItem[]>("list_local_snapshots", { repositoryRoot: repositoryRoot.trim() }),
        invoke<SnapshotTrashEntry[]>("list_local_snapshot_trash", { repositoryRoot: repositoryRoot.trim() }),
        invoke<RepositoryStorageSummary>("get_repository_storage_summary", { repositoryRoot: repositoryRoot.trim() }),
        invoke<RecoveryPoint[]>("list_recovery_points", { repositoryRoot: repositoryRoot.trim() }),
      ]);
      setLocalSnapshots(local);
      setSnapshotTrash(trash);
      setStorageSummary(storage);
      setRecoveryPoints(recovery);
      if (selectedRemoteId && selectedNamespaceId) {
        const [revisions, remoteTrash] = await Promise.all([
          invoke<RevisionSummary[]>("list_remote_revisions", { repositoryRoot: repositoryRoot.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId }),
          invoke<RemoteHistoryTrashOperation[]>("list_remote_history_trash", { repositoryRoot: repositoryRoot.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId }),
        ]);
        setRemoteRevisions(revisions);
        setRemoteHistoryTrash(remoteTrash);
      } else {
        setRemoteRevisions([]);
        setRemoteHistoryTrash([]);
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setHistoryLoading(false);
    }
  }

  async function requestSnapshotTrash(item: LocalSnapshotListItem) {
    try {
      const plan = await invoke<SnapshotDeletionPlan>("plan_snapshot_deletion", {
        repositoryRoot: repositoryRoot.trim(), snapshotId: item.snapshotId,
      });
      setConfirmation({
        title: "将快照移入回收站",
        description: <p>快照清单会进入可恢复回收站。{plan.exclusiveObjectCount} 个独占对象、约 {formatBytes(plan.estimatedReclaimableBytes)} 只有在后续 GC 时才会进入隔离区；共享对象不会删除。</p>,
        confirmLabel: "移入回收站",
        tone: "danger",
        onConfirm: async () => {
          await invoke("trash_local_snapshot", { repositoryRoot: repositoryRoot.trim(), plan });
          setSelectedHistoryId(null);
          await refreshHistory();
        },
      });
    } catch (reason) { setError(String(reason)); }
  }

  async function restoreTrash(entry: SnapshotTrashEntry) {
    try {
      await invoke("restore_trashed_snapshot", { repositoryRoot: repositoryRoot.trim(), operationId: entry.operationId });
      await refreshHistory();
    } catch (reason) { setError(String(reason)); }
  }

  async function inspectGc() {
    try {
      setGcPlan(await invoke<GcPlan>("plan_local_gc", { repositoryRoot: repositoryRoot.trim() }));
    } catch (reason) { setError(String(reason)); }
  }

  async function quarantineGc() {
    if (!gcPlan) return;
    try {
      await invoke("quarantine_local_gc", { repositoryRoot: repositoryRoot.trim(), plan: gcPlan });
      setGcPlan(null);
      await refreshHistory();
    } catch (reason) { setError(String(reason)); }
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

  const setupSteps = [
    { label: "Codex Home", ready: Boolean(codexHome.trim()), detail: codexHome.trim() || "尚未设置", route: "/settings" as AppRoute },
    { label: "远端服务器", ready: profiles.length > 0 && Boolean(selectedRemoteId), detail: selectedProfile?.displayName ?? "尚未配置", route: "/settings" as AppRoute },
    { label: "命名空间", ready: namespaces.length > 0 && Boolean(selectedNamespaceId), detail: selectedNamespace?.displayName ?? "尚未选择", route: "/namespaces" as AppRoute },
  ];
  const setupComplete = setupSteps.every((step) => step.ready);

  const syncStatusPanel = selectedNamespace && namespaceStatus ? <section className="surface sync-workspace">
    <div className="sync-overview-row">
      <div><span className="overline">同步目标</span><h3>{selectedNamespace.displayName}</h3><p>{selectedProfile?.displayName ?? "未选择远端"}</p></div>
      <StatusBadge tone={namespaceStatus.active ? "success" : "warning"}>{namespaceStatus.active ? "当前活动" : "需要切换"}</StatusBadge>
    </div>
    <div className="sync-status-grid">
      <article><span>本机跟踪</span><CopyCode value={namespaceStatus.integratedHead ?? "尚未跟踪"} compact /></article>
      <article><span>远端 Head</span><CopyCode value={namespaceStatus.remoteHead ?? "空命名空间"} compact /></article>
      <article><span>Tracking 代数</span><strong>{namespaceStatus.generation}</strong></article>
      <article><span>写入状态</span><strong>{writeBlockedReason ? "暂不可用" : "已就绪"}</strong></article>
    </div>
    {writeBlockedReason && <div className="inline-alert warning"><AlertTriangle size={17} /><div><strong>写操作尚未就绪</strong><span>{writeBlockedReason}</span></div></div>}
    <div className="primary-action-bar">
      {namespaceStatus.active ? <>
        <button className="button primary action-button" onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowUpFromLine size={18} />推送</button>
        <button className="button primary action-button" onClick={() => void prepareWorkspacePathsAndStart("start_pull_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowDownToLine size={18} />拉取</button>
      </> : !namespaceStatus.activeNamespaceId && !namespaceStatus.remoteHead ?
        <div className="button-row sync-initial-push-actions"><button className="button primary action-button" onClick={() => void start("start_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowUpFromLine size={18} />用本机会话初始化并推送</button><button className="button secondary action-button" onClick={() => void start("start_latest_snapshot_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArchiveRestore size={18} />推送最近一次</button></div> :
        <button className="button warning action-button wide" onClick={() => setConfirmation({
          title: `切换到“${selectedNamespace.displayName}”`,
          description: <><p>应用会先创建备份，再用目标命名空间完整替换本机会话。</p><dl className="confirmation-details"><div><dt>目标</dt><dd>{selectedNamespace.displayName}</dd></div><div><dt>Codex Home</dt><dd>{codexHome}</dd></div></dl></>,
          confirmLabel: "确认备份并切换",
          tone: "warning",
          onConfirm: () => prepareWorkspacePathsAndStart("start_namespace_switch_job", { ...syncPayload, confirmedReplaceLocal: true }),
        })} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><RefreshCw size={18} />切换到此命名空间</button>}
    </div>
  </section> : <section className="surface empty-card"><CircleHelp size={28} /><h3>请选择同步目标</h3><p>先配置远端服务器，然后选择或创建一个命名空间。</p><button className="button primary" onClick={() => navigate("/settings")}>前往设置</button></section>;

  const syncResultPanel = syncReport ? <section className="surface sync-result-panel">
    <div className="section-title"><div><span className="overline">本次运行</span><h3>最近同步结果</h3></div><StatusBadge tone={syncReport.kind === "conflict" ? "warning" : "success"}>{syncReport.kind}</StatusBadge></div>
    <div className="result-summary-grid">
      <article><span>Head</span><CopyCode value={syncReport.head ?? "无 Head"} compact /><small>{syncReport.threadCount} 个会话</small></article>
      <article><span>对象传输</span><strong>↑ {syncReport.uploadedObjects} / ↓ {syncReport.downloadedObjects}</strong><small>{syncReport.pushMetrics ? `${formatBytes(syncReport.pushMetrics.transferredBytes)} · ${(syncReport.pushMetrics.uploadMs / 1000).toFixed(1)} 秒 · ${syncReport.pushMetrics.maxConcurrency} 路并发` : syncReport.checkout ? "已创建本地备份" : "无需本地 checkout"}</small></article>
    </div>
    {syncReport.conflicts.length > 0 && activeConflicts.length === 0 && <div className="inline-alert warning"><AlertTriangle size={17} /><div><strong>冲突上下文已经变化</strong><span>请切回产生冲突的 Home、远端和命名空间后重新拉取。</span></div></div>}
    {activeConflicts.length > 0 && <div className="conflict-workbench modern-conflicts">
      <div className="conflict-summary"><div><strong>需要解决 {activeConflicts.length} 个同线程冲突</strong><span>选择与基础、本地和远端内容指纹绑定；内容改变后旧选择会被拒绝。</span></div><StatusBadge tone="warning">{resolvedConflictCount} / {activeConflicts.length} 已选择</StatusBadge></div>
      <div className="conflict-list">{activeConflicts.map((conflict, index) => {
        const choice = conflictChoices[conflict.conflictId];
        return <article className="conflict-item" key={conflict.conflictId}>
          <header><div><span>冲突 {index + 1}</span><h3>{conflict.title}</h3></div><StatusBadge tone="warning">{conflictKindLabel(conflict.kind)}</StatusBadge></header>
          <CopyCode value={conflict.threadId} />
          <div className="conflict-version-grid">
            <div className="version-card base-version"><span className="version-label">共同基础</span><ConflictVersionDetails version={conflict.base} /></div>
            <button type="button" className={`version-card selectable-version ${choice === "local" ? "chosen-version" : ""}`} onClick={() => setConflictChoices((current) => ({ ...current, [conflict.conflictId]: "local" }))} aria-pressed={choice === "local"} disabled={busy}><span className="version-label">本地版本</span><ConflictVersionDetails version={conflict.local} /><b>{conflict.local ? "保留本地" : "接受本地删除"}</b></button>
            <button type="button" className={`version-card selectable-version ${choice === "remote" ? "chosen-version" : ""}`} onClick={() => setConflictChoices((current) => ({ ...current, [conflict.conflictId]: "remote" }))} aria-pressed={choice === "remote"} disabled={busy}><span className="version-label">远端版本</span><ConflictVersionDetails version={conflict.remote} /><b>{conflict.remote ? "保留远端" : "接受远端删除"}</b></button>
          </div>
        </article>;
      })}</div>
      <div className="sticky-submit"><div><strong>{allConflictsResolved ? "选择完整，可以安全合并" : `还需选择 ${activeConflicts.length - resolvedConflictCount} 项`}</strong><span>提交会先备份并应用到本机，再以当前远端 Head 做 CAS Push。</span></div><button className="button primary" onClick={() => void resolveConflicts()} disabled={busy || !canWrite || !allConflictsResolved}>应用选择并完成合并</button></div>
    </div>}
  </section> : null;

  const sessionReportPanel = report ? <>
    <section className="metric-grid">
      <article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article>
      <article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article>
      <article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article>
      <article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article>
    </section>
    <section className="two-column-grid sessions-grid">
      <article className="surface"><div className="section-title"><h3>会话预览</h3><span>{report.threads.length} / {report.totalCount}</span></div><div className="thread-list">{recentThreads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title}</strong><span title={thread.workspace.sourcePath ?? undefined}>{thread.workspace.sourcePath ?? "未记录工作目录"}</span></div><small>{thread.modelProvider ?? "unknown"}</small></div>)}{recentThreads.length === 0 && <p className="muted-copy">扫描结果没有返回可预览的会话。</p>}</div></article>
      <article className="surface"><div className="section-title"><h3>兼容性状态</h3><span>{report.databasePaths.length} 个数据库</span></div>{quarantineMessage && <div className="inline-alert success"><Check size={17} /><span>{quarantineMessage}</span></div>}{report.warnings.length === 0 ? <div className="inline-alert success"><Check size={17} /><span>扫描完成，没有发现阻塞同步的问题。</span></div> : <div className="warning-list">{report.warnings.map((warning, index) => <div className="warning-row" key={`${warning.path}-${index}`}><div><StatusBadge tone="warning">{warning.kind}</StatusBadge><span>{warning.message}</span><code title={warning.path}>{warning.path}</code></div>{warning.kind === "empty_rollout" && <button type="button" className="button warning small" onClick={() => setConfirmation({ title: "隔离空 Rollout 文件", description: <p>文件会重新校验并移动到隔离目录，不会永久删除。</p>, confirmLabel: "确认隔离", tone: "warning", onConfirm: () => quarantineWarning(warning) })} disabled={busy || !canWrite}>安全清理</button>}</div>)}</div>}</article>
    </section>
  </> : <section className="surface empty-card large"><Database size={30} /><h3>等待首次扫描</h3><p>扫描只读取本机 Codex 会话，不会修改任何数据。</p><button className="button primary" onClick={() => void start("start_scan_job", { codexHome: codexHome.trim() })} disabled={busy || !codexHome.trim() || !isTauriRuntime}>扫描本机会话</button></section>;

  const automaticTools = mappingState ? <section className="surface tool-section">
    <div className="section-title"><div><h3>自动命名空间选择</h3><p>规则和 HMAC 指纹只保存在本机，只负责选择目标，不会自动 checkout。</p></div><label className="switch-control"><input type="checkbox" checked={mappingState.automaticEnabled} onChange={(event) => void setAutomaticSelection(event.target.checked)} disabled={busy} /><span>{mappingState.automaticEnabled ? "已启用" : "已关闭"}</span></label></div>
    <div className="identity-grid">
      <article><span>Provider</span><strong>{mappingState.context.provider ?? "未检测到"}</strong><small>来自 config.toml</small></article>
      <article><span>API Key 指纹</span><strong>{mappingState.context.apiKeyFingerprintHint ?? "不可用"}</strong><small>{apiKeySourceLabel(mappingState.context.apiKeySource)} · 不返回原始 Key</small></article>
      <article><span>Codex Home</span><strong title={mappingState.context.codexHomeKey}>{mappingState.context.codexHomeKey}</strong><small>规范化精确匹配</small></article>
    </div>
    <div className={`mapping-resolution ${mappingState.selection.source === "ambiguous" ? "mapping-ambiguous" : ""}`}><div><span>当前选择来源</span><strong>{selectionSourceLabel(mappingState.selection.source)}</strong><small>{mappingState.selection.selectedNamespaceId ? namespaces.find((namespace) => namespace.id === mappingState.selection.selectedNamespaceId)?.displayName ?? mappingState.selection.selectedNamespaceId : mappingState.selection.source === "ambiguous" ? `${mappingState.selection.ambiguousMappingIds.length} 条同优先级规则指向不同命名空间` : "没有可用目标"}</small></div>{mappingState.selection.source === "manual_override" && <button className="button secondary small" onClick={() => void clearManualOverride()} disabled={busy}>恢复自动选择</button>}</div>
    {mappingState.context.warnings.length > 0 && <div className="mapping-warnings">{mappingState.context.warnings.map((warning, index) => <span key={`${warning}-${index}`}>{warning}</span>)}</div>}
    <div className="mapping-builder">
      <div className="field"><label htmlFor="mapping-label-new">规则名称</label><input id="mapping-label-new" value={mappingLabel} onChange={(event) => setMappingLabel(event.target.value)} placeholder="例如：工作账号" /></div>
      <div className="mapping-criteria"><label><input type="checkbox" checked={matchApiKey} onChange={(event) => setMatchApiKey(event.target.checked)} disabled={!mappingState.context.apiKeyAvailable} />API Key · {mappingState.context.apiKeyFingerprintHint ?? "不可用"}</label><label><input type="checkbox" checked={matchProvider} onChange={(event) => setMatchProvider(event.target.checked)} disabled={!mappingState.context.provider} />Provider · {mappingState.context.provider ?? "不可用"}</label><label><input type="checkbox" checked={matchCodexHome} onChange={(event) => setMatchCodexHome(event.target.checked)} />Codex Home</label></div>
      <button className="button primary" onClick={() => void createMapping()} disabled={busy || !selectedNamespaceId || !mappingLabel.trim() || !mappingCriteriaValid}>创建映射</button>
    </div>
    <div className="mapping-list">{mappingState.mappings.map((mapping) => { const target = namespaces.find((namespace) => namespace.id === mapping.namespaceId); return <article className="mapping-card" key={mapping.id}><div><strong>{mapping.label}</strong><span>→ {target?.displayName ?? mapping.namespaceId}</span></div><div className="mapping-tags">{mapping.matchesApiKey && <code>KEY {mapping.apiKeyFingerprintHint}</code>}{mapping.provider && <code>PROVIDER {mapping.provider}</code>}{mapping.codexHomeKey && <code>HOME {mapping.codexHomeKey}</code>}</div><button className="button danger small" onClick={() => void deleteMapping(mapping.id)} disabled={busy}>删除</button></article>; })}{mappingState.mappings.length === 0 && <p className="muted-copy">尚未创建本机映射规则。</p>}</div>
  </section> : <section className="surface empty-card"><KeyRound size={28} /><h3>请先选择远端服务器</h3><p>自动选择规则按远端分别保存。</p></section>;

  const providerSyncSettings = location.pathname === "/sync" ? <section className="surface settings-card provider-sync-settings">
    <div className="section-title"><div><h3>本地会话同步</h3><p>将现有本机会话切换到 config.toml 当前配置的 provider，不访问服务器。</p></div><KeyRound size={20} /></div>
    <div className="provider-sync-scope" aria-label="Provider 同步范围"><span>同步范围：</span><b>活动会话</b><b>归档会话</b></div>
    <div className="button-row">
      <button className="button secondary" onClick={() => void previewProviderSync()} disabled={busy || !codexHome.trim() || !repositoryRoot.trim()}><RefreshCw size={16} />{providerPreviewActive ? "预览中…" : "预览"}</button>
      <button className="button warning" onClick={() => setConfirmation({ title: "同步本机会话 Provider", description: !providerSyncPreview ? <p>执行阶段会先扫描本机会话，再将需要修改的记录同步到 config.toml 当前配置的 Provider；如果已经一致，任务将以 0 条改变完成。请确认 Codex 已完全退出。</p> : providerSyncPreview.noChanges ? <p>当前预览没有发现需要修改的记录。执行时会重新扫描；如果 Provider 仍然一致，任务将以 0 条改变完成。请确认 Codex 已完全退出。</p> : <p>当前预览发现 {providerSyncPreview.rolloutCount} 个 rollout 和 {providerSyncPreview.databaseRowCount} 条数据库记录需要修改。执行时会重新扫描并先创建备份；请确认 Codex 已完全退出。</p>, confirmLabel: "备份并同步", tone: "warning", onConfirm: () => start("start_provider_sync_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={!canWrite || busy || !codexHome.trim() || !repositoryRoot.trim()}>备份并同步</button>
    </div>
    {providerSyncPreview && <div className={`inline-alert ${providerSyncPreview.noChanges ? "success" : "warning"}`}>{providerSyncPreview.noChanges ? <Check size={17} /> : <AlertTriangle size={17} />}<span>{providerSyncPreview.noChanges ? `当前 provider 为 ${providerSyncPreview.provider}，无需同步` : `目标 ${providerSyncPreview.provider} · ${providerSyncPreview.rolloutCount} 个 rollout（${formatBytes(providerSyncPreview.rolloutBytes)}）· ${providerSyncPreview.databaseRowCount} 条 SQLite 记录`}</span></div>}
    {providerSyncPreview && providerSyncPreview.warnings.length > 0 && <div className="inline-alert warning"><AlertTriangle size={17} /><span>扫描发现 {providerSyncPreview.warnings.length} 条警告；对应文件会保持原样并跳过。</span></div>}
    {providerSyncReport && <div className="inline-alert success"><Check size={17} /><span>{providerSyncReport.rolloutCount === 0 && providerSyncReport.databaseRowCount === 0 ? `检查完成：Provider 已是 ${providerSyncReport.provider}，0 条改变` : `已同步到 ${providerSyncReport.provider}：${providerSyncReport.rolloutCount} 个 rollout、${providerSyncReport.databaseRowCount} 条 SQLite 记录发生改变；备份保存在 ${providerSyncReport.backupDir}`}</span></div>}
  </section> : null;

  const projectTools = selectedNamespace && workspaceMappingState ? <>
    {workspacePathFilterPanel}
    <section className="surface tool-section">
      <div className="section-title"><div><h3>项目路径</h3><p>聚合路径映射、会话使用情况、Codex 项目和空目录状态。</p></div><button className="button secondary" onClick={() => void inspectWorkspaceCleanup()} disabled={busy}><RefreshCw size={16} />刷新状态</button></div>
      {workspaceSetupMessage && <div className="inline-alert success"><Check size={17} /><span>{workspaceSetupMessage}</span></div>}{workspaceCleanupMessage && <div className="inline-alert success"><Check size={17} /><span>{workspaceCleanupMessage}</span></div>}
      <div className="workspace-path-summary"><span>映射规则 {workspaceMappingState.mappings.length}</span><span>Codex 项目 {workspacePathEntries.filter((entry) => entry.codexProjectNames.length > 0).length}</span><span>可清理 {workspaceCleanupReport?.candidates.length ?? 0}</span>{workspaceCleanupReport && workspaceCleanupReport.candidates.length > 0 && <button className="button danger small" onClick={() => { const paths = workspaceCleanupReport.candidates.map((candidate) => candidate.path); setConfirmation({ title: `安全清理 ${paths.length} 个项目路径`, description: <p>普通空目录会移动到隔离区，同时清理残留菜单记录。非空目录不会被处理。</p>, confirmLabel: "确认安全清理", tone: "danger", onConfirm: () => cleanupWorkspaceDirectories(paths) }); }} disabled={busy || !canWrite}>一键安全清理</button>}</div>
      <div className="workspace-path-list">{workspacePathEntries.map((entry) => <article key={entry.path} className={entry.cleanupEligible ? "workspace-path-cleanable" : ""}><div className="workspace-path-main"><code title={entry.path}>{entry.path}</code><div className="workspace-path-badges">{entry.activeCount > 0 && <span className="active-usage">活动 {entry.activeCount}</span>}{entry.archivedCount > 0 && <span className="archived-usage">归档 {entry.archivedCount}</span>}{entry.codexProjectNames.length > 0 && <span className="codex-project-usage">Codex 项目</span>}{entry.directoryState === "empty" && <span className="empty-directory-usage">空目录</span>}{entry.directoryState === "missing" && <span className="missing-directory-usage">目录缺失</span>}{entry.directoryState === "unknown" && <span className="unknown-directory-usage">待扫描</span>}</div></div>{entry.mappings.map((mapping) => <div className="workspace-path-mapping" key={mapping.id}><span>{mapping.inherited ? "继承映射" : "路径映射"}：<code>{mapping.remotePrefix}</code> → <code>{mapping.localPrefix}</code></span>{!mapping.inherited && <button className="button danger small" onClick={() => void deleteWorkspaceMapping(mapping.id)} disabled={busy}>删除映射</button>}</div>)}{entry.codexProjectNames.length > 0 && <small>Codex 项目：{entry.codexProjectNames.join("、")}</small>}{entry.cleanupEligible && <button className="button danger small workspace-path-cleanup-button" onClick={() => setConfirmation({ title: "安全清理项目路径", description: <p>空目录将移动到隔离区，残留菜单记录会在备份后清理。</p>, confirmLabel: "确认安全清理", tone: "danger", onConfirm: () => cleanupWorkspaceDirectories([entry.path]) })} disabled={busy || !canWrite}>安全清理</button>}</article>)}</div>
      <details className="advanced-mapping"><summary>手动添加根路径规则</summary><div className="workspace-mapping-builder"><div className="field"><label htmlFor="remote-workspace-prefix-new">源电脑项目根路径</label><input id="remote-workspace-prefix-new" value={remoteWorkspacePrefix} onChange={(event) => setRemoteWorkspacePrefix(event.target.value)} placeholder="例如 D:\projects" /></div><div className="field"><label htmlFor="local-workspace-prefix-new">当前电脑项目根路径</label><div className="path-picker-row"><input id="local-workspace-prefix-new" value={localWorkspacePrefix} onChange={(event) => setLocalWorkspacePrefix(event.target.value)} placeholder="例如 F:\workspace" /><button type="button" className="button secondary" onClick={() => void selectLocalWorkspacePrefix()} disabled={busy || !isTauriRuntime}>选择目录</button></div></div><button className="button primary" onClick={() => void createWorkspaceMapping()} disabled={busy || !remoteWorkspacePrefix.trim() || !localWorkspacePrefix.trim()}>添加规则</button></div></details>
      {workspaceMappingState.mappings.length > 0 && <div className="inline-action"><div><strong>将规则应用到已有会话</strong><span>操作会先安全备份，再重新整理当前活动命名空间。</span></div><button className="button secondary" onClick={() => setConfirmation({ title: "将路径规则应用到已有会话", description: <p>应用会创建备份并修改当前活动命名空间中的会话路径。</p>, confirmLabel: "备份并应用", tone: "warning", onConfirm: () => start("start_workspace_remap_job", syncPayload) })} disabled={busy || !canWrite || !namespaceStatus?.active}>应用规则</button></div>}
    </section>
    <section className="surface tool-section"><div className="section-title"><div><h3>会话项目路径迁移</h3><p>批量处理尚未适配当前电脑的项目路径，已有映射和可用路径不会改动。</p></div><button className="button secondary" onClick={() => void inspectWorkspaceMigration()} disabled={busy}>检查项目路径</button></div>{migrationMessage && <div className="inline-alert success"><Check size={17} /><span>{migrationMessage}</span></div>}{migrationPlan && migrationPlan.unmappedPaths.length > 0 ? <><div className="migration-summary"><strong>待迁移 {migrationPlan.unmappedPaths.length} 项</strong><span>{migrationPlan.mappedPathCount} 项已有映射 · {migrationPlan.existingPathCount} 项原路径可用</span></div><WorkspacePathEditor parentDirectory={migrationParent} drafts={migrationDrafts} busy={busy} submitLabel="保存路径并迁移" onParentChange={(value) => changeEditorParent("migration", value)} onTargetChange={(index, value) => setMigrationDrafts((current) => current.map((draft, candidate) => candidate === index ? { ...draft, localPath: value } : draft))} onChooseParent={() => void chooseEditorParent("migration")} onChooseTarget={(index) => void chooseEditorTarget("migration", index)} onSubmit={() => void saveMigrationDrafts()} /></> : migrationPlan && <div className="inline-alert success"><Check size={17} /><span>所有项目路径均已映射或在本机可用。</span></div>}</section>
  </> : <section className="surface empty-card"><FolderCog size={28} /><h3>请先选择命名空间</h3><p>项目路径规则按 Home、远端和命名空间隔离。</p></section>;

  const snapshotTools = <section className="surface tool-section">
    <div className="section-title"><div><h3>本地快照与恢复</h3><p>用于诊断、手动导入和未完成操作恢复；所有写入仍遵守 Codex 关闭检查。</p></div><button className="button primary" onClick={() => void start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true })} disabled={busy || !canWrite}>创建本地快照</button></div>
    <div className="field"><label htmlFor="manifest-path-new">快照清单路径</label><input id="manifest-path-new" value={manifestPath} onChange={(event) => setManifestPath(event.target.value)} placeholder="~/.codex-session-sync/snapshots/<id>.json" /></div>
    <div className="button-row"><button className="button secondary" onClick={() => void start("start_validation_job", { manifestPath: manifestPath.trim(), repositoryRoot: repositoryRoot.trim() })} disabled={busy || !manifestPath.trim() || !isTauriRuntime}>验证快照</button><button className="button danger" onClick={() => setConfirmation({ title: "增量导入快照", description: <p>导入会先备份当前会话，并在校验失败时自动回滚。请确认 Codex 已完全退出。</p>, confirmLabel: "确认备份并导入", tone: "danger", onConfirm: () => start("start_import_job", { manifestPath: manifestPath.trim(), codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={busy || !manifestPath.trim() || !canWrite}>增量导入</button></div>
    <div className="divider" />
    <div className="recovery-row"><div className="field"><label htmlFor="journal-path-new">未完成操作的 Journal 路径</label><div className="path-picker-row"><input id="journal-path-new" value={journalPath} onChange={(event) => setJournalPath(event.target.value)} placeholder="选择 checkout-*.json" /><button type="button" className="button secondary" onClick={() => void selectJournalFile()} disabled={busy || !isTauriRuntime}>选择文件</button></div></div><button className="button warning" onClick={() => setConfirmation({ title: "从备份恢复", description: <p>恢复会根据 Journal 校验当前状态并还原备份。请确认 Codex 已完全退出。</p>, confirmLabel: "确认恢复", tone: "warning", onConfirm: () => start("start_recovery_job", { journalPath: journalPath.trim(), confirmedCodexClosed: true }) })} disabled={busy || !journalPath.trim() || !canWrite}>从备份恢复</button></div>
    {(snapshot || validation || importReport || recoveredJournal) && <div className="result-summary-grid tool-results">{snapshot && <article><span>最新快照</span><strong>{snapshot.threadCount} 个会话</strong><small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small></article>}{validation && <article><span>验证结果</span><strong>{validation.valid ? "完整有效" : "验证失败"}</strong><small>{validation.snapshotId}</small></article>}{importReport && <article><span>导入完成</span><strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong><small title={importReport.backupDir}>已创建备份</small></article>}{recoveredJournal && <article><span>恢复结果</span><strong>{recoveredJournal.status}</strong><small>{recoveredJournal.error ?? recoveredJournal.operationId}</small></article>}</div>}
  </section>;

  const localVersionRows: VersionRow[] = localSnapshots.map((item) => ({
    id: item.snapshotId,
    title: item.metadata.description || (item.metadata.automatic ? "自动安全快照" : "本地快照"),
    createdAt: item.createdAt,
    threadCount: item.threadCount,
    logicalBytes: item.logicalBytes,
    labels: [item.metadata.pinned ? "PINNED" : "SNAPSHOT", ...item.metadata.tags],
    kind: "local",
  }));
  const remoteVersionRows: VersionRow[] = remoteRevisions.map((item, index) => ({
    id: item.revisionId,
    title: index === 0 ? "远端最新版本" : "远端版本",
    createdAt: item.createdAt,
    threadCount: item.threadCount,
    logicalBytes: item.logicalBytes,
    labels: index === 0 ? ["HEAD"] : [],
    kind: "remote",
  }));
  const syncVersionRows: VersionRow[] = [
    ...localVersionRows.slice(0, 3),
    ...remoteVersionRows,
  ].sort((left, right) => right.createdAt.localeCompare(left.createdAt));
  const selectedLocalSnapshot = localSnapshots.find((item) => item.snapshotId === selectedHistoryId) ?? null;
  const selectedRemoteRevision = remoteRevisions.find((item) => item.revisionId === selectedHistoryId) ?? null;
  const historyPage = <div className="page-stack history-page">
    <PageIntro title="快照与恢复" description="以版本图方式浏览本地快照和远端命名空间历史；删除先进入回收站，对象回收另行确认。" action={<div className="button-row"><button className="button secondary" onClick={() => void refreshHistory()} disabled={historyLoading}><RefreshCw size={15} />刷新</button><button className="button primary" onClick={() => void start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true })} disabled={busy || !canWrite}>创建快照</button></div>} />
    <section className="history-workbench surface">
      <aside className="history-tree">
        <strong>来源</strong>
        <button className={historySource === "local" ? "active" : ""} onClick={() => { setHistorySource("local"); setSelectedHistoryId(null); }}><Database size={15} />本机 <b>{localSnapshots.length}</b></button>
        <button className={historySource === "remote" ? "active" : ""} onClick={() => { setHistorySource("remote"); setSelectedHistoryId(null); }} disabled={!selectedNamespaceId}><Server size={15} />{selectedNamespace?.displayName ?? "远端命名空间"} <b>{remoteRevisions.length}</b></button>
        <button className={historySource === "recovery" ? "active" : ""} onClick={() => { setHistorySource("recovery"); setSelectedHistoryId(null); }}><RotateCcw size={15} />操作恢复 <b>{recoveryPoints.filter((point) => point.requiresAttention).length}</b></button>
        <button className={historySource === "trash" ? "active" : ""} onClick={() => { setHistorySource("trash"); setSelectedHistoryId(null); }}><Trash2 size={15} />回收站 <b>{snapshotTrash.length + remoteHistoryTrash.filter((entry) => entry.state === "active").length}</b></button>
        <div className="history-tree-divider" />
        <button onClick={() => void inspectGc()}><ArchiveRestore size={15} />对象 GC</button>
      </aside>
      <div className="history-main">
        <div className="history-toolbar"><strong>{historySource === "local" ? "本地快照" : historySource === "remote" ? "远端 Revision" : historySource === "recovery" ? "操作恢复点" : "可恢复删除"}</strong><span>{historyLoading ? "正在读取…" : "按创建时间倒序"}</span></div>
        {historySource === "local" && <VersionGraphTable rows={localVersionRows} selectedId={selectedHistoryId} onSelect={(row) => setSelectedHistoryId(row.id)} />}
        {historySource === "remote" && <VersionGraphTable rows={remoteVersionRows} selectedId={selectedHistoryId} onSelect={(row) => setSelectedHistoryId(row.id)} />}
        {historySource === "recovery" && <div className="trash-list">{recoveryPoints.map((point) => <article key={point.operationId} className={point.requiresAttention ? "requires-attention" : ""}><div><strong>{point.kind === "checkout" ? "语义切换" : point.kind === "provider_sync" ? "Provider 同步" : "增量导入"} · {point.status}</strong><span>{point.updatedAt ? new Date(point.updatedAt).toLocaleString("zh-CN") : point.journalPath}</span></div>{point.requiresAttention && <button className="button warning small" onClick={() => { setJournalPath(point.journalPath); setConfirmation({ title: "恢复未完成操作", description: <p>将根据 Journal 和备份重新校验后恢复。Codex 必须完全退出。</p>, confirmLabel: "确认恢复", tone: "warning", onConfirm: () => start("start_recovery_job", { journalPath: point.journalPath, confirmedCodexClosed: true }) }); }} disabled={!canWrite || busy}><RotateCcw size={14} />处理恢复</button>}</article>)}{recoveryPoints.length === 0 && <div className="version-log-empty">没有发现操作恢复点</div>}</div>}
        {historySource === "trash" && <div className="trash-list">{snapshotTrash.map((entry) => <article key={entry.operationId}><div><strong>本地快照 · {shortHead(entry.snapshotId)}</strong><span>{new Date(entry.trashedAt).toLocaleString("zh-CN")}</span></div><button className="button secondary small" onClick={() => void restoreTrash(entry)}><RotateCcw size={14} />恢复快照</button></article>)}{remoteHistoryTrash.filter((entry) => entry.state === "active").map((entry) => <article key={entry.operationId}><div><strong>远端历史 · {entry.revisionCount} 个版本</strong><span>{shortHead(entry.oldHead)} → {shortHead(entry.newHead)} · 到期 {new Date(entry.expiresAt).toLocaleDateString("zh-CN")}</span></div><button className="button secondary small" onClick={async () => { await invoke("restore_remote_history_trash", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, operationId: entry.operationId }); await refreshNamespaces(); await refreshHistory(); }}><RotateCcw size={14} />恢复远端历史</button></article>)}{snapshotTrash.length === 0 && remoteHistoryTrash.every((entry) => entry.state !== "active") && <div className="version-log-empty">回收站为空</div>}</div>}
        {(selectedLocalSnapshot || selectedRemoteRevision) && <section className="version-details">
          <div><span className="overline">选中版本</span><h3>{selectedLocalSnapshot ? (selectedLocalSnapshot.metadata.description || "本地快照") : "远端 Revision"}</h3><CopyCode value={selectedHistoryId ?? ""} /></div>
          <div className="version-detail-metrics"><span>会话 <b>{selectedLocalSnapshot?.threadCount ?? selectedRemoteRevision?.threadCount}</b></span><span>逻辑大小 <b>{formatBytes(selectedLocalSnapshot?.logicalBytes ?? selectedRemoteRevision?.logicalBytes ?? 0)}</b></span><span>物理引用 <b>{formatBytes(selectedLocalSnapshot?.physicalReferencedBytes ?? selectedRemoteRevision?.physicalReferencedBytes ?? 0)}</b></span></div>
          {selectedLocalSnapshot && <div className="button-row"><button className="button warning" onClick={() => setConfirmation({ title: "语义恢复本地快照", description: <p>当前 Codex 会话将先完整备份并写入 Journal，再按线程语义恢复所选快照；Provider、工作区路径和 rollout 换行格式会按当前机器物化。失败时会自动回滚。</p>, confirmLabel: "备份并恢复", tone: "warning", onConfirm: () => start("start_snapshot_restore_job", { manifestPath: selectedLocalSnapshot.manifestPath, codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={!canWrite || busy}>语义恢复</button><button className="button secondary" onClick={async () => { await invoke("update_snapshot_metadata", { repositoryRoot: repositoryRoot.trim(), snapshotId: selectedLocalSnapshot.snapshotId, metadata: { ...selectedLocalSnapshot.metadata, pinned: !selectedLocalSnapshot.metadata.pinned } }); await refreshHistory(); }}>{selectedLocalSnapshot.metadata.pinned ? "取消固定" : "固定快照"}</button><button className="button danger" onClick={() => void requestSnapshotTrash(selectedLocalSnapshot)} disabled={selectedLocalSnapshot.metadata.pinned}><Trash2 size={15} />移入回收站</button></div>}
          {selectedRemoteRevision && <div className="button-row"><button className="button secondary" onClick={() => void start("start_remote_revision_download_job", { repositoryRoot: repositoryRoot.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId })} disabled={busy}>下载为本地快照</button><button className="button secondary" onClick={() => setConfirmation({ title: "恢复为本地待推送状态", description: <p>当前会话会先备份并通过 Journal 精确切换到该远端版本；Tracking 仍保留当前远端 Head，之后可普通 Push 发布。</p>, confirmLabel: "备份并恢复", tone: "warning", onConfirm: () => start("start_remote_revision_restore_job", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId, publish: false, confirmedCodexClosed: true }) })} disabled={busy || !canWrite}>恢复为待 Push</button><button className="button warning" onClick={() => setConfirmation({ title: "恢复并发布为新版本", description: <p>先安全恢复所选历史内容，再以当前远端 Head 为父版本发布新的 Revision；不会改写已有历史。</p>, confirmLabel: "恢复并发布", tone: "warning", onConfirm: () => start("start_remote_revision_restore_job", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId, publish: true, confirmedCodexClosed: true }) })} disabled={busy || !canWrite}>恢复并发布</button>{remoteRevisions[0]?.revisionId !== selectedRemoteRevision.revisionId && <button className="button danger" onClick={() => setConfirmation({ title: "回退远端 Head", description: <p>该版本之后的远端历史会进入 30 天可恢复回收站，Namespace Epoch 将递增。对象不会立即删除。</p>, confirmLabel: "确认回退 Head", tone: "danger", onConfirm: async () => { await invoke("truncate_remote_history", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, newHead: selectedRemoteRevision.revisionId }); await refreshNamespaces(); await refreshHistory(); } })}>回退 Head 到此处</button>}{remoteRevisions[0]?.revisionId === selectedRemoteRevision.revisionId && <button className="button danger" onClick={() => setConfirmation({ title: "删除当前远端 Head", description: <p>当前 Head 会进入 30 天可恢复回收站，父版本成为新 Head；共享对象和内容不会立即删除。</p>, confirmLabel: "删除当前 Head", tone: "danger", onConfirm: async () => { await invoke("truncate_remote_history", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, newHead: selectedRemoteRevision.parentRevision }); await refreshNamespaces(); await refreshHistory(); } })}>删除当前 Head</button>}</div>}
        </section>}
      </div>
    </section>
    {storageSummary && <section className="surface storage-summary" aria-label="仓库存储统计"><div><span>仓库占用</span><strong>{formatBytes(storageSummary.repositoryPhysicalBytes)}</strong></div><div><span>活动可达</span><strong>{formatBytes(storageSummary.activePhysicalBytes)}</strong></div><div><span>共享对象</span><strong>{formatBytes(storageSummary.sharedPhysicalBytes)}</strong></div><div><span>回收站保护</span><strong>{formatBytes(storageSummary.trashBytes)}</strong></div><div><span>可隔离</span><strong>{formatBytes(storageSummary.reclaimableBytes)}</strong></div><div><span>已隔离</span><strong>{formatBytes(storageSummary.gcQuarantineBytes)}</strong></div></section>}
    {gcPlan && <section className="surface gc-panel"><div><h3>GC 隔离计划</h3><p>{gcPlan.unreachableObjects.length} 个全局不可达对象，可释放约 {formatBytes(gcPlan.reclaimableBytes)}。执行后先移入隔离区，不会永久删除。</p></div><div className="button-row"><button className="button secondary" onClick={() => setGcPlan(null)}>关闭</button><button className="button danger" onClick={() => setConfirmation({ title: "隔离不可达对象", description: <p>计划会在执行前重新计算；仍被任何活动快照、回收站快照或远端 Revision 缓存引用的对象不会移动。</p>, confirmLabel: "确认隔离", tone: "danger", onConfirm: quarantineGc })} disabled={gcPlan.unreachableObjects.length === 0}>隔离对象</button></div></section>}
  </div>;

  return <AppShell processes={processes} busy={busy} onRefreshProcesses={() => void refreshProcesses()}>
    {processes.length > 0 && <div className="global-process-alert" role="status"><AlertTriangle size={18} /><div><strong>检测到 Codex 正在运行</strong><span>扫描和配置仍可使用；同步、导入、恢复和清理暂时禁用。</span><div className="process-chips">{processes.map((process) => <code key={process.pid}>{process.kind} · {process.name} · PID {process.pid}</code>)}</div></div></div>}
    <Routes>
      <Route path="/" element={<Navigate to="/overview" replace />} />
      <Route path="/overview" element={<div className="page-stack">
        <PageIntro title="欢迎使用 Codex Session Sync" description="通过自托管服务器，在多台电脑之间安全同步 Codex 会话。" action={<button className="button primary" onClick={() => navigate(setupComplete ? "/sync" : setupSteps.find((step) => !step.ready)?.route ?? "/sync")}>{setupComplete ? "开始同步" : "继续配置"}<ArrowRight size={16} /></button>} />
        <section className={`surface readiness-card ${setupComplete ? "complete" : ""}`}><div className="readiness-heading"><div><span className="overline">当前状态</span><h3>{setupComplete ? "同步环境已就绪" : "完成以下配置即可开始"}</h3><p>{workflowNextStep}</p></div><StatusBadge tone={setupComplete ? "success" : "warning"}>{setupComplete ? "准备完成" : `${setupSteps.filter((step) => step.ready).length} / ${setupSteps.length}`}</StatusBadge></div><div className="setup-steps">{setupSteps.map((step, index) => <button key={step.label} type="button" onClick={() => navigate(step.route)}><span className={`step-index ${step.ready ? "ready" : ""}`}>{step.ready ? <Check size={15} /> : index + 1}</span><span><strong>{step.label}</strong><small title={step.detail}>{step.detail}</small></span><ChevronRight size={16} /></button>)}</div></section>
        <section className="overview-grid"><article className="surface overview-card"><div className="section-title"><h3>当前同步上下文</h3><button className="text-button" onClick={() => navigate("/sync")}>查看同步</button></div><dl className="summary-list"><div><dt>Codex Home</dt><dd title={codexHome}>{codexHome || "未设置"}</dd></div><div><dt>远端服务器</dt><dd>{selectedProfile?.displayName ?? "未选择"}</dd></div><div><dt>命名空间</dt><dd>{selectedNamespace?.displayName ?? "未选择"}</dd></div></dl></article><article className="surface overview-card"><div className="section-title"><h3>本机会话</h3><button className="text-button" onClick={() => navigate("/sessions")}>查看会话</button></div>{report ? <div className="overview-metrics"><div><strong>{report.totalCount}</strong><span>总会话</span></div><div><strong>{formatBytes(report.totalRolloutBytes)}</strong><span>Rollout</span></div><div><strong>{report.warnings.length}</strong><span>警告</span></div></div> : <p className="muted-copy">尚未扫描本机会话。</p>}</article><article className="surface overview-card"><div className="section-title"><h3>最近同步</h3>{syncReport && <StatusBadge tone={syncReport.kind === "conflict" ? "warning" : "success"}>{syncReport.kind}</StatusBadge>}</div>{syncReport ? <div className="latest-sync"><strong>{syncReport.threadCount} 个会话</strong><CopyCode value={syncReport.head ?? "无 Head"} compact /><span>↑ {syncReport.uploadedObjects} · ↓ {syncReport.downloadedObjects}</span></div> : <p className="muted-copy">当前运行尚无同步结果。</p>}</article></section>
      </div>} />
      <Route path="/sync" element={<div className="page-stack compact-stack">
        <PageIntro title="同步会话" description="明确选择同步方向；写入前会再次检查 Codex 进程和项目路径。" />
        {providerSyncSettings}
        <section className="surface context-selector"><div className="field"><label>Codex Home</label><button className="selector-display" onClick={() => navigate("/settings")} title={codexHome}>{codexHome || "未设置"}<Settings size={15} /></button></div><div className="field"><label htmlFor="sync-remote">远端服务器</label><select id="sync-remote" value={selectedRemoteId} onChange={(event) => setSelectedRemoteId(event.target.value)} disabled={busy}><option value="">请选择远端</option>{profiles.map((profile) => <option key={profile.id} value={profile.id}>{profile.displayName}</option>)}</select></div><div className="field"><label htmlFor="sync-namespace">命名空间</label><select id="sync-namespace" value={selectedNamespaceId} onChange={(event) => void chooseNamespace(event.target.value)} disabled={busy || !selectedRemoteId}><option value="">请选择命名空间</option>{namespaces.map((namespace) => <option key={namespace.id} value={namespace.id}>{namespace.displayName}</option>)}</select>{mappingState && <small>{selectionSourceLabel(mappingState.selection.source)}</small>}</div></section>
        {syncStatusPanel}<section className="surface sync-version-log"><div className="section-title"><div><h3>版本图谱</h3><p>本地快照与当前远端命名空间共享同一种 IDEA 风格版本日志。</p></div><button className="text-button" onClick={() => navigate("/history")}>打开快照与恢复</button></div><VersionGraphTable rows={syncVersionRows} selectedId={selectedHistoryId} onSelect={(row) => { setSelectedHistoryId(row.id); navigate("/history"); setHistorySource(row.kind); }} /></section>{syncResultPanel}
      </div>} />
      <Route path="/history" element={historyPage} />
      <Route path="/sessions" element={<div className="page-stack"><PageIntro title="本机会话" description="扫描会在后台运行，只读取会话和兼容性信息。" action={<button className="button primary" onClick={() => void start("start_scan_job", { codexHome: codexHome.trim() })} disabled={busy || !codexHome.trim() || !isTauriRuntime}><RefreshCw size={16} />重新扫描</button>} />{sessionReportPanel}</div>} />
      <Route path="/namespaces" element={<div className="page-stack"><PageIntro title="命名空间" description="每个命名空间拥有独立历史，可重命名但稳定 ID 不变。" action={<button className="button secondary" onClick={() => void refreshNamespaces()} disabled={busy || !selectedRemoteId}><RefreshCw size={16} />刷新</button>} />{selectedRemoteId ? <><section className="namespace-list">{namespaces.map((namespace) => { const active = namespaceStatus?.activeNamespaceId === namespace.id; const selected = selectedNamespaceId === namespace.id; return <article key={namespace.id} className={`surface namespace-list-card ${selected ? "selected" : ""}`}><div><div className="namespace-name"><h3>{namespace.displayName}</h3>{active && <StatusBadge tone="success">当前活动</StatusBadge>}</div><CopyCode value={namespace.head ?? "空命名空间"} compact /></div><div className="namespace-card-actions"><button className="button secondary small" onClick={() => void chooseNamespace(namespace.id)} disabled={busy}>{selected ? "已选为目标" : "设为同步目标"}</button></div></article>; })}{namespaces.length === 0 && <section className="surface empty-card"><Database size={28} /><h3>服务器上还没有命名空间</h3><p>创建第一个命名空间后即可推送本机会话。</p></section>}</section><section className="surface namespace-editor-card"><div><h3>{selectedNamespace ? "重命名选中项" : "创建命名空间"}</h3><p>名称可以随时修改，不影响同步身份。</p></div><div className="namespace-editor"><div className="field"><label htmlFor="namespace-name-new">命名空间名称</label><input id="namespace-name-new" value={namespaceName} onChange={(event) => setNamespaceName(event.target.value)} placeholder="例如：工作会话" /></div><div className="button-row"><button className="button primary" onClick={() => void createNamespace()} disabled={busy || !namespaceName.trim()}><Plus size={16} />创建</button><button className="button secondary" onClick={() => void renameNamespace()} disabled={busy || !selectedNamespaceId || !namespaceName.trim()}>保存新名称</button></div></div></section></> : <section className="surface empty-card"><Server size={28} /><h3>请先配置远端服务器</h3><p>命名空间存储在远端服务器中。</p><button className="button primary" onClick={() => navigate("/settings")}>前往设置</button></section>}</div>} />
      <Route path="/settings" element={<div className="page-stack"><PageIntro title="设置" description="配置本机数据位置、远端服务器和界面外观。" /><section className="settings-grid"><article className="surface settings-card"><div className="section-title"><div><h3>本机存储</h3><p>路径变化会刷新对应的远端与同步状态。</p></div><Database size={20} /></div><div className="field"><label htmlFor="codex-home-new">Codex Home</label><input id="codex-home-new" value={codexHome} onChange={(event) => setCodexHome(event.target.value)} disabled={busy} /></div><div className="field"><label htmlFor="repository-root-new">本地同步仓库</label><input id="repository-root-new" value={repositoryRoot} onChange={(event) => setRepositoryRoot(event.target.value)} disabled={busy} /></div></article><article className="surface settings-card"><div className="section-title"><div><h3>外观</h3><p>默认跟随操作系统，也可以固定主题。</p></div>{resolvedTheme === "dark" ? <Moon size={20} /> : <Sun size={20} />}</div><div className="theme-options" role="radiogroup" aria-label="主题"><button role="radio" aria-checked={themePreference === "system"} className={themePreference === "system" ? "selected" : ""} onClick={() => setThemePreference("system")}><RefreshCw size={17} /><span><strong>跟随系统</strong><small>当前为{resolvedTheme === "dark" ? "深色" : "浅色"}</small></span></button><button role="radio" aria-checked={themePreference === "light"} className={themePreference === "light" ? "selected" : ""} onClick={() => setThemePreference("light")}><Sun size={17} /><span><strong>浅色</strong><small>始终使用浅色界面</small></span></button><button role="radio" aria-checked={themePreference === "dark"} className={themePreference === "dark" ? "selected" : ""} onClick={() => setThemePreference("dark")}><Moon size={17} /><span><strong>深色</strong><small>始终使用深色界面</small></span></button></div></article></section>{providerSyncSettings}<section className="surface settings-card remote-settings"><div className="section-title"><div><h3>远端服务器</h3><p>Bearer Token 只保存到操作系统凭据库，前端不会读回明文。</p></div><StatusBadge>{profiles.length} 个配置</StatusBadge></div><div className="profile-tabs">{profiles.map((profile) => <button key={profile.id} className={selectedRemoteId === profile.id ? "selected" : ""} onClick={() => setSelectedRemoteId(profile.id)} disabled={busy}>{profile.displayName}</button>)}<button onClick={() => { setSelectedRemoteId(""); setRemoteName("个人服务器"); setRemoteUrl("http://127.0.0.1:8787"); setRemoteToken(""); setNamespaces([]); setSelectedNamespaceId(""); setMappingState(null); setWorkspaceMappingState(null); }} disabled={busy}><Plus size={15} />新建远端</button></div><div className="remote-form"><div className="field"><label htmlFor="remote-name-new">配置名称</label><input id="remote-name-new" value={remoteName} onChange={(event) => setRemoteName(event.target.value)} /></div><div className="field"><label htmlFor="remote-url-new">服务器 URL</label><input id="remote-url-new" value={remoteUrl} onChange={(event) => setRemoteUrl(event.target.value)} /></div><div className="field"><label htmlFor="remote-token-new">Bearer Token</label><input id="remote-token-new" type="password" value={remoteToken} onChange={(event) => setRemoteToken(event.target.value)} placeholder={selectedProfile?.credentialConfigured ? "已保存；留空则不修改" : "至少 16 位可见 ASCII 字符"} /></div></div><div className="button-row"><button className="button primary" onClick={() => void saveRemote()} disabled={busy || !remoteName.trim() || !remoteUrl.trim() || (!selectedRemoteId && !remoteToken.trim())}>保存并验证</button><button className="button secondary" onClick={() => void testConnection()} disabled={busy || !selectedRemoteId}>测试连接</button></div>{(selectedProfile?.insecureHttp || remoteUrl.trim().startsWith("http://")) && <div className="inline-alert warning"><AlertTriangle size={17} /><span>当前连接未使用 HTTPS，仅建议在本机或可信内网使用。</span></div>}{connectionMessage && <div className="inline-alert success"><Check size={17} /><span>{connectionMessage}</span></div>}</section></div>} />
      <Route path="/advanced" element={<Navigate to="/advanced/automatic" replace />} />
      <Route path="/advanced/*" element={<div className="page-stack"><PageIntro title="高级工具" description="这些工具用于自动化选择、跨电脑路径适配和手动恢复。" /><nav className="subnavigation" aria-label="高级工具分类"><NavLink to="/advanced/automatic">自动选择映射</NavLink><NavLink to="/advanced/projects">项目路径</NavLink><NavLink to="/history">快照与恢复</NavLink></nav>{location.pathname === "/advanced/projects" ? projectTools : location.pathname === "/advanced/snapshots" ? snapshotTools : automaticTools}</div>} />
      <Route path="*" element={<Navigate to="/overview" replace />} />
    </Routes>

    {pendingWorkspaceSync && <div className="dialog-backdrop" role="presentation"><section className="workspace-path-modal" role="dialog" aria-modal="true" aria-label="设置本机项目路径"><div className="workspace-modal-heading"><div><span className="overline">同步前路径检查</span><h2>设置本机项目路径</h2></div><button type="button" className="icon-button" onClick={() => setPendingWorkspaceSync(null)} disabled={busy} aria-label="关闭"><X size={19} /></button></div><p>远端会话引用了当前电脑尚不可用的项目路径。选择统一父目录后仍可逐项修改。</p><div className="migration-summary"><strong>{pendingWorkspaceSync.plan.unmappedPaths.length} 项待设置</strong><span>{pendingWorkspaceSync.plan.mappedPathCount} 项已有映射 · {pendingWorkspaceSync.plan.existingPathCount} 项原路径可用</span></div><WorkspacePathEditor parentDirectory={workspaceEditorParent} drafts={workspaceDrafts} busy={busy} submitLabel="保存路径并继续" onParentChange={(value) => changeEditorParent("sync", value)} onTargetChange={(index, value) => setWorkspaceDrafts((current) => current.map((draft, candidate) => candidate === index ? { ...draft, localPath: value } : draft))} onChooseParent={() => void chooseEditorParent("sync")} onChooseTarget={(index) => void chooseEditorTarget("sync", index)} onSubmit={() => void saveWorkspaceDraftsAndContinue()} onCancel={() => setPendingWorkspaceSync(null)} /></section></div>}
    <ConfirmDialog request={confirmation} onClose={() => setConfirmation(null)} />
    <ErrorDialog message={error} onClose={() => setError(null)} />
    {job && <aside className={`task-center ${jobFailure ? "failed" : ""}`} aria-live="polite"><div className="task-center-heading"><div><span className="overline">{job.kind} · {job.state}</span><strong>{jobFailure ? "任务失败" : job.progress.phase.replaceAll("_", " ")}</strong></div>{!isActive(job) && <button className="icon-button" onClick={() => setJob(null)} aria-label="关闭任务"><X size={17} /></button>}</div><p>{jobFailure ?? job.progress.message}</p><div className={`progress-track ${progressPercent === null ? "indeterminate" : ""}`}><div className="progress-fill" style={{ width: progressPercent === null ? undefined : `${progressPercent}%` }} /></div><div className="task-center-footer"><small>{progressDetail}</small>{isActive(job) && <button className="button danger small" onClick={() => void cancelCurrentJob()} disabled={!job.cancellable || job.state === "cancelling"}>{job.state === "cancelling" ? "正在安全停止…" : job.cancellable ? "取消任务" : "当前阶段不可取消"}</button>}</div></aside>}
  </AppShell>;

}
