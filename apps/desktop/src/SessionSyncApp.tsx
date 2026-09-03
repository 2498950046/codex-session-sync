import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { Navigate, Route, Routes, useLocation, useNavigate } from "./router";
import {
  AlertTriangle,
  ArchiveRestore,
  ArrowDownToLine,
  ArrowRight,
  ArrowUpFromLine,
  Check,
  ChevronLeft,
  ChevronRight,
  CircleHelp,
  Copy,
  Database,
  Download,
  FolderCog,
  Folder,
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
  UserRound,
  X,
} from "lucide-react";
import desktopPackage from "../package.json";
import { AppShell } from "./AppShell";
import { useTheme } from "./theme";
import type {
  AutomaticWorkspaceMappingResult,
  ChangeKind,
  CheckoutReport,
  CodexProcess,
  ImportReport,
  JobSnapshot,
  LocalBackupDeletionResult,
  LocalBackupItem,
  LocalSnapshotListItem,
  LocalTrashPurgePlan,
  LocalTrashPurgeResult,
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
  RemoteHistoryTrashPurgeResult,
  RevisionSummary,
  ScanReport,
  ScanWarning,
  SnapshotSummary,
  SnapshotDeletionPlan,
  SnapshotTrashEntry,
  SnapshotValidationReport,
  StagingCandidatePreview,
  StagingPlanReport,
  SyncReport,
  ThreadBundle,
  ThreadMessagesPage,
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

export type AppRoute = "/overview" | "/sync" | "/sessions" | "/settings" | "/me";

type AvailableUpdate = {
  version: string;
  date: string | null;
  notes: string | null;
};

type RuntimeUpdate = {
  version: string;
  date?: string;
  body?: string;
  downloadAndInstall: (onEvent?: (event: { event: string; data: { contentLength?: number; chunkLength?: number } }) => void) => Promise<void>;
};

const UPDATE_DISMISS_STORAGE_KEY = "codex-session-sync.dismissed-update-version";
const PROJECT_URL = "https://github.com/2498950046/codex-session-sync";

function UpdateDetails({ update, currentVersion, progress, onInstall, installing }: {
  update: AvailableUpdate | null;
  currentVersion: string;
  progress: string | null;
  onInstall: () => void;
  installing: boolean;
}) {
  if (!update) return <div className="update-empty"><Check size={25} /><strong>已是最新版本</strong><span>当前版本 {currentVersion}</span></div>;
  return <div className="update-details">
    <div className="update-version-row"><div><span className="overline">发现新版本</span><h3>v{update.version}</h3><p>当前版本 v{currentVersion}{update.date ? ` · 发布于 ${new Date(update.date).toLocaleString("zh-CN")}` : ""}</p></div><StatusBadge tone="success">可更新</StatusBadge></div>
    <div className="release-notes"><strong>更新内容</strong><p>{update.notes?.trim() || "此版本未提供更新说明，请前往 GitHub Release 查看详情。"}</p></div>
    {progress && <div className="inline-alert"><RefreshCw size={17} /><span>{progress}</span></div>}
    <div className="button-row"><button type="button" className="button primary" onClick={onInstall} disabled={installing}><Download size={16} />{installing ? "正在更新…" : "立即更新并重启"}</button></div>
  </div>;
}

function UpdatePrompt({ update, currentVersion, onDismiss, onOpenUpdates, onInstall, installing }: {
  update: AvailableUpdate | null;
  currentVersion: string;
  onDismiss: () => void;
  onOpenUpdates: () => void;
  onInstall: () => void;
  installing: boolean;
}) {
  if (!update) return null;
  return <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !installing) onDismiss(); }}>
    <section className="update-prompt" role="dialog" aria-modal="true" aria-labelledby="update-prompt-title">
      <div className="dialog-icon success"><Download size={22} /></div>
      <div className="dialog-copy"><span className="overline">软件更新</span><h2 id="update-prompt-title">发现新版本 v{update.version}</h2><p>当前版本为 v{currentVersion}。{update.notes?.trim() || "新版本已在 GitHub Releases 发布。"}</p></div>
      <div className="dialog-actions"><button type="button" className="button secondary" onClick={onDismiss} disabled={installing}>暂不更新</button><button type="button" className="button secondary" onClick={onOpenUpdates} disabled={installing}>查看详情</button><button type="button" className="button primary" onClick={onInstall} disabled={installing}><Download size={15} />{installing ? "下载中…" : "立即更新"}</button></div>
    </section>
  </div>;
}

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

function syncOutcomeLabel(kind: SyncReport["kind"]): string {
  return {
    pushed: "已推送",
    pulled: "已拉取",
    merged: "已合并",
    switched: "已切换",
    remapped: "已重映射",
    no_changes: "无需同步",
    conflict: "需要处理冲突",
  }[kind];
}

function syncOutcomeTone(kind: SyncReport["kind"]): "neutral" | "success" | "warning" {
  if (kind === "conflict") return "warning";
  return kind === "no_changes" ? "neutral" : "success";
}

function FolderPager({ page, pageCount, total, onChange }: { page: number; pageCount: number; total: number; onChange: (page: number) => void }) {
  return <div className="session-pagination"><button className="button secondary small" onClick={() => onChange(Math.max(1, page - 1))} disabled={page <= 1}>上一页</button><span>第 {page} / {pageCount} 页 · {total} 条</span><button className="button secondary small" onClick={() => onChange(Math.min(pageCount, page + 1))} disabled={page >= pageCount}>下一页</button></div>;
}

function SessionFolderBrowser({ report, onAction, onProjectAction, onOpen, mutating }: {
  report: ScanReport;
  onAction: (thread: ThreadBundle, action: "archive" | "restore" | "delete") => void;
  onProjectAction: (threads: ThreadBundle[], action: "archive" | "restore" | "delete") => void;
  onOpen: (thread: ThreadBundle) => void;
  mutating: boolean;
}) {
  const [status, setStatus] = useState<"active" | "archived" | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [pages, setPages] = useState({ root: 1, status: 1, project: 1 });
  const pageSize = 10;
  const statusThreads = report.threads.filter((thread) => thread.archived === (status === "archived"));
  const projects = new Map<string, ThreadBundle[]>();
  statusThreads.forEach((thread) => {
    const key = thread.workspace.logicalId || thread.workspace.sourcePath || "无项目";
    projects.set(key, [...(projects.get(key) ?? []), thread]);
  });
  const projectThreads = project ? projects.get(project) ?? [] : [];
  const level = !status ? "root" : !project ? "status" : "project";
  const page = pages[level];
  const setPage = (next: number) => setPages((current) => ({ ...current, [level]: next }));
  const pageCount = Math.max(1, Math.ceil(projectThreads.length / pageSize));
  const projectEntries = [...projects.entries()];
  const projectPageCount = Math.max(1, Math.ceil(projectEntries.length / pageSize));
  const visibleProjects = projectEntries.slice((page - 1) * pageSize, page * pageSize);
  const rootEntries: Array<["active" | "archived", number]> = [["active", report.activeCount], ["archived", report.archivedCount]];
  const rootPageCount = Math.max(1, Math.ceil(rootEntries.length / pageSize));
  const visibleRootEntries = rootEntries.slice((page - 1) * pageSize, page * pageSize);
  const visibleThreads = projectThreads.slice((page - 1) * pageSize, page * pageSize);
  const levelSummary = !status
    ? `${2} 个分类 · 共 ${report.threads.length} 个会话`
    : !project
      ? `${projects.size} 个项目 · 共 ${report.threads.length} 个会话`
      : `${projectThreads.length} 个会话 · 共 ${report.threads.length} 个会话`;
  const enterStatus = (next: "active" | "archived") => { setStatus(next); setProject(null); };
  const enterProject = (next: string) => { setProject(next); };

  return <article className={`surface session-folder-browser ${mutating ? "mutating" : ""}`} aria-busy={mutating}>
    <div className="section-title"><div><h3>会话浏览</h3><p>按文件夹逐级进入</p></div><span>{levelSummary}</span></div>
    <fieldset className="session-folder-fieldset" disabled={mutating}>
    <nav className="session-breadcrumb" aria-label="会话层级">
      <button type="button" onClick={() => { setStatus(null); setProject(null); }}>会话</button>
      {status && <><ChevronRight size={13} /><button type="button" onClick={() => setProject(null)}>{status === "active" ? "活动" : "归档"}</button></>}
      {project && <><ChevronRight size={13} /><span>{project}</span></>}
    </nav>
    {!status && <><div className="folder-grid">{visibleRootEntries.map(([kind, count]) => <button type="button" className="folder-entry" key={kind} onClick={() => enterStatus(kind)}><Folder size={24} /><span><strong>{kind === "active" ? "活动" : "归档"}</strong><small>{count} 个会话</small></span><ChevronRight size={17} /></button>)}</div><FolderPager page={page} pageCount={rootPageCount} total={rootEntries.length} onChange={setPage} /></>}
    {status && !project && <><button type="button" className="folder-back" onClick={() => setStatus(null)}><ChevronLeft size={15} />返回上一级</button><div className="folder-grid">{visibleProjects.map(([name, threads]) => <div className="folder-entry" key={name}><button type="button" className="folder-open" onClick={() => enterProject(name)}><Folder size={24} /><span><strong>{name}</strong><small>{threads.length} 个会话</small></span><ChevronRight size={17} /></button><div className="folder-actions"><button className="button secondary small" onClick={() => onProjectAction(threads, status === "active" ? "archive" : "restore")}>{status === "active" ? "归档项目" : "恢复项目"}</button><button className="button danger small" onClick={() => onProjectAction(threads, "delete")}>删除项目</button></div></div>)}{projects.size === 0 && <p className="muted-copy">这一层没有会话。</p>}</div><FolderPager page={page} pageCount={projectPageCount} total={projectEntries.length} onChange={setPage} /></>}
    {status && project && <><button type="button" className="folder-back" onClick={() => setProject(null)}><ChevronLeft size={15} />返回项目列表</button><div className="thread-list folder-thread-list">{visibleThreads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title || "未命名会话"}</strong><span>{thread.workspace.sourcePath ?? "未记录工作目录"}</span><small>{thread.modelProvider ?? "unknown"}</small></div><div className="thread-actions"><button className="button secondary small" onClick={() => onOpen(thread)}>查看</button><button className="button secondary small" onClick={() => onAction(thread, status === "active" ? "archive" : "restore")}>{status === "active" ? "归档" : "恢复"}</button><button className="button danger small" onClick={() => onAction(thread, "delete")}>删除</button></div></div>)}</div><FolderPager page={page} pageCount={pageCount} total={projectThreads.length} onChange={setPage} /></>}
    </fieldset>
  </article>;
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

function VersionGraphTable({ rows, selectedId, onSelect, loadingLabel }: {
  rows: VersionRow[];
  selectedId: string | null;
  onSelect: (row: VersionRow) => void;
  loadingLabel?: string | null;
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
    {loadingLabel ? <div className="version-log-loading" role="status" aria-live="polite"><RefreshCw size={18} /><span>{loadingLabel}</span></div> : rows.length === 0 && <div className="version-log-empty">暂无版本记录</div>}
  </div>;
}

function HistoryLoading({ label }: { label: string }) {
  return <div className="history-action-loading" role="status" aria-live="polite"><RefreshCw size={18} /><span>{label}</span></div>;
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

function StagingDialog({ plan, selected, showAll, busy, actionLabel, onToggle, onSetAll, onShowAll, onClose, onPush }: {
  plan: StagingPlanReport | null;
  selected: Set<string>;
  showAll: boolean;
  busy: boolean;
  actionLabel: string;
  onToggle: (ids: string[], checked: boolean) => void;
  onSetAll: (checked: boolean) => void;
  onShowAll: (value: boolean) => void;
  onClose: () => void;
  onPush: () => void;
}) {
  const [status, setStatus] = useState<"active" | "archived" | "deleted" | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [pages, setPages] = useState({ root: 1, status: 1, project: 1 });
  if (!plan) return null;
  const candidates = plan.candidates.filter((candidate) => showAll || candidate.kind !== "unchanged");
  const visibleForStatus = (candidate: StagingCandidatePreview) => status === "deleted"
    ? candidate.kind === "deleted"
    : candidate.kind !== "deleted" && candidate.archived === (status === "archived");
  const statusCandidates = status ? candidates.filter(visibleForStatus) : [];
  const projects = new Map<string, StagingCandidatePreview[]>();
  statusCandidates.forEach((candidate) => {
    const key = candidate.workspace.logicalId || candidate.workspace.sourcePath || "无项目";
    projects.set(key, [...(projects.get(key) ?? []), candidate]);
  });
  const projectCandidates = project ? projects.get(project) ?? [] : [];
  const level = !status ? "root" : !project ? "status" : "project";
  const page = pages[level];
  const setPage = (next: number) => setPages((current) => ({ ...current, [level]: next }));
  const pageSize = 10;
  const pageItems = <T,>(items: T[]) => items.slice((page - 1) * pageSize, page * pageSize);
  const pager = (total: number) => <FolderPager page={page} pageCount={Math.max(1, Math.ceil(total / pageSize))} total={total} onChange={setPage} />;
  const changed = plan.candidates.filter((candidate) => candidate.kind !== "unchanged");
  const selectedChanged = changed.filter((candidate) => selected.has(candidate.threadId));
  const selectedBytes = selectedChanged.reduce((total, candidate) => total + candidate.byteLength, 0);
  const label = (kind: ChangeKind) => ({ added: "新增", modified: "修改", archive_changed: "归档/恢复", deleted: "待删除", unchanged: "未变化" })[kind];
  const toggle = (items: StagingCandidatePreview[], checked: boolean) => onToggle(items.filter((item) => item.kind !== "unchanged").map((item) => item.threadId), checked);
  const checkState = (items: StagingCandidatePreview[]) => {
    const changeable = items.filter((item) => item.kind !== "unchanged");
    const count = changeable.filter((item) => selected.has(item.threadId)).length;
    return { checked: changeable.length > 0 && count === changeable.length, partial: count > 0 && count < changeable.length, count, total: changeable.length };
  };
  const Check = ({ items }: { items: StagingCandidatePreview[] }) => {
    const state = checkState(items);
    return <label className="staging-check" title={state.partial ? `已选 ${state.count}/${state.total}` : undefined}><input type="checkbox" checked={state.checked} ref={(element) => { if (element) element.indeterminate = state.partial; }} onChange={(event) => toggle(items, event.target.checked)} disabled={busy || state.total === 0} /><span>{state.partial ? `${state.count}/${state.total}` : ""}</span></label>;
  };
  const root = [
    { key: "active" as const, title: "活动", items: candidates.filter((item) => item.kind !== "deleted" && !item.archived) },
    { key: "archived" as const, title: "归档", items: candidates.filter((item) => item.kind !== "deleted" && item.archived) },
    { key: "deleted" as const, title: "待删除", items: candidates.filter((item) => item.kind === "deleted") },
  ].filter((entry) => entry.items.length > 0);

  return <div className="dialog-backdrop" role="presentation">
    <section className="staging-dialog" role="dialog" aria-modal="true" aria-labelledby="staging-dialog-title">
      <header><div><span className="overline">Git 式暂存区</span><h2 id="staging-dialog-title">选择要推送的变化</h2><p>未勾选的远端会话会保留在完整 Revision 中，不会被删除。</p></div><button type="button" className="icon-button" onClick={onClose} disabled={busy} aria-label="关闭"><X size={18} /></button></header>
      {plan.warningCount > 0 && <div className="inline-alert warning"><AlertTriangle size={17} /><span>本地扫描发现 {plan.warningCount} 个兼容性警告，无法安全推送。</span></div>}
      <div className="staging-toolbar"><label><input type="checkbox" checked={showAll} onChange={(event) => onShowAll(event.target.checked)} disabled={busy} />显示全部（含未变化）</label><div><button type="button" className="button secondary small" onClick={() => onSetAll(true)} disabled={busy || changed.length === 0}>全选变化</button><button type="button" className="button secondary small" onClick={() => onSetAll(false)} disabled={busy || selected.size === 0}>清空</button></div></div>
      <div className="staging-summary"><span>已暂存 <b>{selectedChanged.length}</b> 项</span><span>预计上传 ≤ <b>{formatBytes(selectedBytes)}</b></span><span>新增 {selectedChanged.filter((item) => item.kind === "added").length} · 修改 {selectedChanged.filter((item) => item.kind === "modified").length} · 删除 {selectedChanged.filter((item) => item.kind === "deleted").length}</span></div>
      <nav className="session-breadcrumb" aria-label="暂存层级"><button type="button" onClick={() => { setStatus(null); setProject(null); }}>变化</button>{status && <><ChevronRight size={13} /><button type="button" onClick={() => setProject(null)}>{status === "active" ? "活动" : status === "archived" ? "归档" : "待删除"}</button></>}{project && <><ChevronRight size={13} /><span>{project}</span></>}</nav>
      {!status && <div className="staging-body"><div className="folder-grid">{pageItems(root).map((entry) => <div className="folder-entry" key={entry.key}><Check items={entry.items} /><button type="button" className="folder-open" onClick={() => { setStatus(entry.key); setProject(null); }}><Folder size={24} /><span><strong>{entry.title}</strong><small>{entry.items.length} 项变化</small></span><ChevronRight size={17} /></button></div>)}</div>{pager(root.length)}</div>}
      {status && !project && <div className="staging-body"><button type="button" className="folder-back" onClick={() => setStatus(null)}><ChevronLeft size={15} />返回上一级</button><div className="folder-grid">{pageItems([...projects.entries()]).map(([name, items]) => <div className="folder-entry" key={name}><Check items={items} /><button type="button" className="folder-open" onClick={() => setProject(name)}><Folder size={24} /><span><strong>{name}</strong><small>{items.length} 项变化</small></span><ChevronRight size={17} /></button></div>)}</div>{pager(projects.size)}</div>}
      {status && project && <div className="staging-body"><button type="button" className="folder-back" onClick={() => setProject(null)}><ChevronLeft size={15} />返回项目列表</button><div className="thread-list folder-thread-list">{pageItems(projectCandidates).map((candidate) => <label className="thread-row staging-thread" key={candidate.threadId}><input type="checkbox" checked={selected.has(candidate.threadId)} onChange={(event) => onToggle([candidate.threadId], event.target.checked)} disabled={busy || candidate.kind === "unchanged"} /><div><strong>{candidate.title || "未命名会话"}</strong><span>{candidate.workspace.sourcePath ?? "未记录工作目录"}</span><small>{label(candidate.kind)} · {candidate.modelProvider ?? "unknown"}</small></div></label>)}</div>{pager(projectCandidates.length)}</div>}
      <footer><button type="button" className="button secondary" onClick={onClose} disabled={busy}>取消</button><button type="button" className="button primary" onClick={onPush} disabled={busy || selectedChanged.length === 0 || plan.warningCount > 0}><ArrowUpFromLine size={16} />{actionLabel}</button></footer>
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

function backupCategoryLabel(category: LocalBackupItem["category"]) {
  if (category === "provider_sync") return "Provider 同步";
  if (category === "import") return "快照导入";
  if (category === "checkout") return "切换与恢复";
  if (category === "workspace_cleanup") return "项目清理";
  return "其他备份";
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
  const [sessionMutating, setSessionMutating] = useState(false);
  const [sessionActionMessage, setSessionActionMessage] = useState<string | null>(null);
  const [selectedThread, setSelectedThread] = useState<ThreadBundle | null>(null);
  const [threadMessages, setThreadMessages] = useState<ThreadMessagesPage | null>(null);
  const [threadMessagesLoading, setThreadMessagesLoading] = useState(false);
  const [sessionPage, setSessionPage] = useState(1);
  const [sessionTab, setSessionTab] = useState<"sessions" | "compatibility">("sessions");
  const [snapshot, setSnapshot] = useState<SnapshotSummary | null>(null);
  const [localSnapshots, setLocalSnapshots] = useState<LocalSnapshotListItem[]>([]);
  const [remoteRevisions, setRemoteRevisions] = useState<RevisionSummary[]>([]);
  const [snapshotTrash, setSnapshotTrash] = useState<SnapshotTrashEntry[]>([]);
  const [remoteHistoryTrash, setRemoteHistoryTrash] = useState<RemoteHistoryTrashOperation[]>([]);
  const [selectedHistoryId, setSelectedHistoryId] = useState<string | null>(null);
  const [historySource, setHistorySource] = useState<"all" | "local" | "remote" | "recovery" | "trash" | "backup">("all");
  const previewKind = import.meta.env.DEV ? new URLSearchParams(window.location.search).get("preview") : null;
  const [settingsTab, setSettingsTab] = useState<"local" | "remote" | "advanced" | "project" | "provider" | "snapshots">(previewKind === "mapping" ? "advanced" : "local");
  const [myTab, setMyTab] = useState<"home" | "updates">("home");
  const [availableUpdate, setAvailableUpdate] = useState<AvailableUpdate | null>(null);
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);
  const [updateChecking, setUpdateChecking] = useState(false);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const [updatePromptOpen, setUpdatePromptOpen] = useState(false);
  const updateRef = useRef<RuntimeUpdate | null>(null);
  useEffect(() => {
    document.body.dataset.settingsTab = location.pathname === "/settings" ? settingsTab : "";
    return () => { delete document.body.dataset.settingsTab; };
  }, [location.pathname, settingsTab]);
  const [historyOpen, setHistoryOpen] = useState(previewKind === "history");
  const [advancedOpen, setAdvancedOpen] = useState(previewKind === "mapping");
  const [storageSummary, setStorageSummary] = useState<RepositoryStorageSummary | null>(null);
  const [recoveryPoints, setRecoveryPoints] = useState<RecoveryPoint[]>([]);
  const [localBackups, setLocalBackups] = useState<LocalBackupItem[]>([]);
  const [selectedBackupIds, setSelectedBackupIds] = useState<Set<string>>(new Set());
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyLoadingLabel, setHistoryLoadingLabel] = useState("正在读取快照…");
  const [validation, setValidation] = useState<SnapshotValidationReport | null>(null);
  const [importReport, setImportReport] = useState<ImportReport | null>(null);
  const [providerSyncPreview, setProviderSyncPreview] = useState<ProviderSyncPreview | null>(null);
  const [providerPreviewLoading, setProviderPreviewLoading] = useState(false);
  const [providerSyncReport, setProviderSyncReport] = useState<ProviderSyncReport | null>(null);
  const [recoveredJournal, setRecoveredJournal] = useState<OperationJournal | null>(null);
  const [syncReport, setSyncReport] = useState<SyncReport | null>(null);
  const [syncReportTargetKey, setSyncReportTargetKey] = useState<string | null>(null);
  const [stagingPlan, setStagingPlan] = useState<StagingPlanReport | null>(null);
  const [stagedThreadIds, setStagedThreadIds] = useState<Set<string>>(new Set());
  const [stagingShowAll, setStagingShowAll] = useState(false);
  const [stagingAction, setStagingAction] = useState<"push" | "snapshot">("push");
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

  async function checkForUpdate(manual = false) {
    if (!isTauriRuntime || isDevelopmentPreview) {
      if (manual) setUpdateMessage("开发预览模式不会连接 GitHub Releases；请在已安装的软件中检查更新。");
      return;
    }
    setUpdateChecking(true);
    setUpdateMessage(manual ? "正在检查 GitHub Releases…" : null);
    try {
      const { check } = await import("@tauri-apps/plugin-updater");
      const candidate = await check() as unknown as RuntimeUpdate | null;
      updateRef.current = candidate;
      if (!candidate) {
        setAvailableUpdate(null);
        setUpdateMessage(`已是最新版本（v${desktopPackage.version}）。`);
        return;
      }
      const summary: AvailableUpdate = {
        version: candidate.version,
        date: candidate.date ?? null,
        notes: candidate.body ?? null,
      };
      setAvailableUpdate(summary);
      setUpdateMessage(`发现新版本 v${summary.version}。`);
      if (!manual && window.localStorage.getItem(UPDATE_DISMISS_STORAGE_KEY) !== summary.version) setUpdatePromptOpen(true);
    } catch (reason) {
      updateRef.current = null;
      setUpdateMessage(`检查更新失败：${String(reason)}`);
    } finally {
      setUpdateChecking(false);
    }
  }

  function dismissUpdatePrompt() {
    if (availableUpdate) window.localStorage.setItem(UPDATE_DISMISS_STORAGE_KEY, availableUpdate.version);
    setUpdatePromptOpen(false);
  }

  function openUpdates() {
    setUpdatePromptOpen(false);
    setMyTab("updates");
    navigate("/me");
  }

  async function installUpdate() {
    const candidate = updateRef.current;
    if (!candidate) {
      setUpdateMessage("更新信息已过期，请重新检查后再安装。");
      return;
    }
    setUpdateInstalling(true);
    setUpdateMessage("正在下载更新…");
    try {
      let downloaded = 0;
      let total: number | undefined;
      await candidate.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength;
          setUpdateMessage(total ? `正在下载更新（0 / ${Math.ceil(total / 1024 / 1024)} MB）…` : "正在下载更新…");
        } else if (event.event === "Progress") {
          downloaded += event.data.chunkLength ?? 0;
          setUpdateMessage(total ? `正在下载更新（${Math.min(100, Math.round(downloaded / total * 100))}%）…` : "正在下载更新…");
        } else if (event.event === "Finished") {
          setUpdateMessage("更新已验证，正在安装并重启…");
        }
      });
      const { relaunch } = await import("@tauri-apps/plugin-process");
      await relaunch();
    } catch (reason) {
      setUpdateMessage(`更新未完成：${String(reason)}`);
      setUpdateInstalling(false);
    }
  }

  useEffect(() => { void checkForUpdate(); }, []);

  const busy = isActive(job) || remoteLoading || providerPreviewLoading;
  const providerPreviewActive = providerPreviewLoading
    || (job?.kind === "provider_sync_preview" && isActive(job));
  const canWrite = processes.length === 0 && isTauriRuntime;
  const sessionPageSize = 8;
  const sessionPageCount = Math.max(1, Math.ceil((report?.threads.length ?? 0) / sessionPageSize));
  const recentThreads = useMemo(() => {
    const threads = report?.threads ?? [];
    return threads.slice((sessionPage - 1) * sessionPageSize, sessionPage * sessionPageSize);
  }, [report, sessionPage]);
  useEffect(() => { setSessionPage((page) => Math.min(page, sessionPageCount)); }, [sessionPageCount]);
  useEffect(() => {
    if (report) setSessionTab(report.warnings.length > 0 ? "compatibility" : "sessions");
  }, [report?.codexHome, report?.warnings.length]);
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
    if (location.pathname !== "/sync") return;
    void refreshHistory();
  }, [location.pathname, repositoryRoot, selectedRemoteId, selectedNamespaceId]);

  useEffect(() => {
    if (!historyOpen) return;
    void refreshHistory("正在刷新完整版本图谱…");
  }, [historyOpen]);

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
    if (completed.kind === "staging-plan") {
      setStagingPlan(result as StagingPlanReport);
      setStagedThreadIds(new Set());
      setStagingShowAll(false);
    }
    if (completed.kind === "staged-snapshot") {
      const summary = result as SnapshotSummary;
      setSnapshot(summary);
      setManifestPath(summary.manifestPath);
      await refreshHistory();
    }
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
    if (["push", "staged-push", "pull", "resolve", "switch", "remap"].includes(completed.kind)) {
      const synced = result as SyncReport;
      setSyncReport(synced);
      setSyncReportTargetKey(jobSyncTargets.current.get(completed.jobId) ?? syncTargetKey);
      jobSyncTargets.current.delete(completed.jobId);
      setConflictChoices({});
      if (synced.checkout) setJournalPath(synced.checkout.journalPath);
      await refreshNamespaces();
      await refreshNamespaceStatus();
      if (completed.kind === "push" || completed.kind === "staged-push") await refreshHistory();
      if (completed.kind !== "push" && completed.kind !== "staged-push") {
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
      if (["start_push_job", "start_staged_push_job", "start_pull_job", "start_conflict_resolution_job", "start_namespace_switch_job", "start_workspace_remap_job"].includes(command)) {
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

  async function refreshHistory(label = "正在读取快照…") {
    if (!isTauriRuntime || !repositoryRoot.trim()) return;
    setHistoryLoading(true);
    setHistoryLoadingLabel(label);
    try {
      const [local, trash, storage, recovery, backups] = await Promise.all([
        invoke<LocalSnapshotListItem[]>("list_local_snapshots", { repositoryRoot: repositoryRoot.trim() }),
        invoke<SnapshotTrashEntry[]>("list_local_snapshot_trash", { repositoryRoot: repositoryRoot.trim() }),
        invoke<RepositoryStorageSummary>("get_repository_storage_summary", { repositoryRoot: repositoryRoot.trim() }),
        invoke<RecoveryPoint[]>("list_recovery_points", { repositoryRoot: repositoryRoot.trim() }),
        invoke<LocalBackupItem[]>("list_local_backups", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim() }),
      ]);
      setLocalSnapshots(local);
      setSnapshotTrash(trash);
      setStorageSummary(storage);
      setRecoveryPoints(recovery);
      setLocalBackups(backups);
      setSelectedBackupIds((current) => new Set([...current].filter((id) => backups.some((backup) => backup.id === id && backup.deletable))));
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

  function toggleBackupSelection(id: string) {
    setSelectedBackupIds((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function requestBackupDeletion() {
    const selected = localBackups.filter((backup) => selectedBackupIds.has(backup.id));
    if (selected.length === 0) return;
    const totalBytes = selected.reduce((total, backup) => total + backup.byteCount, 0);
    setConfirmation({
      title: "删除选中的本地备份",
      description: <p>将永久删除 {selected.length} 个备份，预计释放 {formatBytes(totalBytes)}。删除后对应操作将不能再使用这些文件回滚，此操作不可撤销。</p>,
      confirmLabel: "永久删除",
      tone: "danger",
      onConfirm: async () => {
        setHistoryLoading(true);
        setHistoryLoadingLabel("正在删除本地备份…");
        try {
          const result = await invoke<LocalBackupDeletionResult>("delete_local_backups", {
            repositoryRoot: repositoryRoot.trim(),
            codexHome: codexHome.trim(),
            backupIds: selected.map((backup) => backup.id),
          });
          setSelectedBackupIds(new Set());
          await refreshHistory(`已删除 ${result.deletedCount} 个备份，释放 ${formatBytes(result.freedBytes)}`);
        } catch (reason) {
          setError(String(reason));
        } finally {
          setHistoryLoading(false);
        }
      },
    });
  }

  async function requestSnapshotTrash(item: LocalSnapshotListItem) {
    setHistoryLoading(true);
    setHistoryLoadingLabel("正在准备删除快照…");
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
          setHistoryLoading(true);
          setHistoryLoadingLabel("正在移入回收站…");
          await invoke("trash_local_snapshot", { repositoryRoot: repositoryRoot.trim(), plan });
          setSelectedHistoryId(null);
          await refreshHistory("正在更新快照列表…");
        },
      });
    } catch (reason) { setError(String(reason)); }
    finally { setHistoryLoading(false); }
  }

  async function restoreTrash(entry: SnapshotTrashEntry) {
    setHistoryLoading(true);
    setHistoryLoadingLabel("正在恢复快照…");
    try {
      await invoke("restore_trashed_snapshot", { repositoryRoot: repositoryRoot.trim(), operationId: entry.operationId });
      await refreshHistory("正在更新快照列表…");
    } catch (reason) { setError(String(reason)); }
    finally { setHistoryLoading(false); }
  }

  async function requestLocalTrashPurge(operationIds: string[], purgeAll: boolean) {
    setHistoryLoading(true);
    setHistoryLoadingLabel("正在计算永久删除范围…");
    try {
      const plan = await invoke<LocalTrashPurgePlan>("plan_local_trash_purge", {
        repositoryRoot: repositoryRoot.trim(), operationIds, purgeAll,
      });
      const estimatedBytes = plan.objectReclaimableBytes + plan.trashMetadataBytes;
      setConfirmation({
        title: purgeAll ? "清空本地回收站" : "永久删除本地快照",
        description: <p>将永久删除 {plan.trashEntryCount} 个恢复点，预计释放 {formatBytes(estimatedBytes)}。仍被其他快照或远端缓存引用的 {formatBytes(plan.retainedSharedBytes)} 会保留；此操作不可撤销。</p>,
        confirmLabel: purgeAll ? "确认清空" : "永久删除",
        tone: "danger",
        onConfirm: async () => {
          setHistoryLoading(true);
          setHistoryLoadingLabel("正在永久删除本地回收站内容…");
          const result = await invoke<LocalTrashPurgeResult>("purge_local_trash", {
            repositoryRoot: repositoryRoot.trim(), plan,
          });
          setSelectedHistoryId(null);
          await refreshHistory(`已删除 ${result.deletedTrashEntries} 个本地恢复点，释放 ${formatBytes(result.freedBytes)}`);
        },
      });
    } catch (reason) { setError(String(reason)); }
    finally { setHistoryLoading(false); }
  }

  async function restoreRemoteHistoryTrash(entry: RemoteHistoryTrashOperation) {
    setHistoryLoading(true);
    setHistoryLoadingLabel("正在恢复远端历史…");
    try {
      await invoke("restore_remote_history_trash", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, operationId: entry.operationId });
      await refreshNamespaces();
      await refreshHistory("正在更新远端历史…");
    } catch (reason) { setError(String(reason)); }
    finally { setHistoryLoading(false); }
  }

  function requestRemoteTrashPurge(operationIds: string[], purgeAll: boolean) {
    const count = purgeAll
      ? remoteHistoryTrash.filter((entry) => entry.state === "active").length
      : operationIds.length;
    setConfirmation({
      title: purgeAll ? "清空远端回收站" : "永久删除远端历史",
      description: <p>将永久删除 {count} 个远端历史恢复点。服务端只会删除不再被任何命名空间引用的对象；此操作不可撤销。</p>,
      confirmLabel: purgeAll ? "确认清空" : "永久删除",
      tone: "danger",
      onConfirm: async () => {
        setHistoryLoading(true);
        setHistoryLoadingLabel("正在永久删除远端历史…");
        try {
          const result = await invoke<RemoteHistoryTrashPurgeResult>("purge_remote_history_trash", {
            repositoryRoot: repositoryRoot.trim(), remoteId: selectedRemoteId,
            namespaceId: selectedNamespaceId, operationIds, purgeAll,
          });
          await refreshHistory(`已删除 ${result.purgedOperationCount} 个远端恢复点，释放 ${formatBytes(result.reclaimedBytes)}`);
        } catch (reason) { setError(String(reason)); }
        finally { setHistoryLoading(false); }
      },
    });
  }

  async function truncateRemoteHistory(newHead: string | null, label: string) {
    setHistoryLoading(true);
    setHistoryLoadingLabel(label);
    try {
      await invoke("truncate_remote_history", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, newHead });
      await refreshNamespaces();
      await refreshHistory("正在更新远端历史…");
    } catch (reason) { setError(String(reason)); }
    finally { setHistoryLoading(false); }
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

  async function openStaging(action: "push" | "snapshot" = "push") {
    if (!selectedRemoteId || !selectedNamespaceId) return;
    setStagingPlan(null);
    setStagedThreadIds(new Set());
    setStagingAction(action);
    await start("start_staging_plan_job", {
      repositoryRoot: repositoryRoot.trim(),
      codexHome: codexHome.trim(),
      remoteId: selectedRemoteId,
      namespaceId: selectedNamespaceId,
    });
  }

  function toggleStaged(ids: string[], checked: boolean) {
    setStagedThreadIds((current) => {
      const next = new Set(current);
      ids.forEach((id) => checked ? next.add(id) : next.delete(id));
      return next;
    });
  }

  function submitStaged() {
    if (!stagingPlan) return;
    const selected = [...stagedThreadIds];
    setStagingPlan(null);
    void start(stagingAction === "push" ? "start_staged_push_job" : "start_staged_snapshot_job", {
      request: {
        ...syncPayload,
        baseRevisionId: stagingPlan.baseRevisionId,
        selectedThreadIds: selected,
      },
    });
  }

  const setupSteps = [
    { label: "Codex Home", ready: Boolean(codexHome.trim()), detail: codexHome.trim() || "尚未设置", route: "/settings" as AppRoute },
    { label: "远端服务器", ready: profiles.length > 0 && Boolean(selectedRemoteId), detail: selectedProfile?.displayName ?? "尚未配置", route: "/settings" as AppRoute },
    { label: "命名空间", ready: namespaces.length > 0 && Boolean(selectedNamespaceId), detail: selectedNamespace?.displayName ?? "尚未选择", route: "/settings" as AppRoute },
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
        <button className="button primary action-button" onClick={() => void openStaging("push")} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowUpFromLine size={18} />选择并推送</button>
        <button className="button primary action-button" onClick={() => void prepareWorkspacePathsAndStart("start_pull_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowDownToLine size={18} />拉取</button>
      </> : !namespaceStatus.activeNamespaceId && !namespaceStatus.remoteHead ?
        <div className="button-row sync-initial-push-actions"><button className="button primary action-button" onClick={() => void openStaging("push")} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArrowUpFromLine size={18} />选择并初始化推送</button><button className="button secondary action-button" onClick={() => void start("start_latest_snapshot_push_job", syncPayload)} disabled={busy || !canWrite} title={writeBlockedReason ?? undefined}><ArchiveRestore size={18} />推送最近一次</button></div> :
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
    <div className="section-title"><div><span className="overline">本次运行</span><h3>最近同步结果</h3></div><StatusBadge tone={syncOutcomeTone(syncReport.kind)}>{syncOutcomeLabel(syncReport.kind)}</StatusBadge></div>
    <div className="result-summary-grid">
      <article><span>Head</span><CopyCode value={syncReport.head ?? "无 Head"} compact /><small>{syncReport.threadCount} 个会话</small></article>
      <article><span>对象传输</span><strong>↑ {syncReport.uploadedObjects} / ↓ {syncReport.downloadedObjects}</strong><small>{syncReport.pushMetrics ? `${formatBytes(syncReport.pushMetrics.transferredBytes)} · ${(syncReport.pushMetrics.uploadMs / 1000).toFixed(1)} 秒 · ${syncReport.pushMetrics.maxConcurrency} 路并发` : syncReport.checkout ? "已创建本地备份" : "无需本地 checkout"}</small></article>
    </div>
    {syncReport.kind === "no_changes" && <div className="inline-alert"><Check size={17} /><span>本地与远端 Head 的语义内容已经一致；本次没有上传对象，也没有创建新的远端 Revision。</span></div>}
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

  const requestSessionAction = (thread: ThreadBundle, action: "archive" | "restore" | "delete") => {
    const deleting = action === "delete";
    setConfirmation({
      title: deleting ? "物理删除会话" : action === "archive" ? "归档会话" : "恢复会话",
      description: deleting
        ? <p>会话及其本地 rollout、数据库记录将被永久删除，无法恢复。请确认已完全退出 Codex。</p>
        : <p>将“{thread.title || thread.threadId}”{action === "archive" ? "移入归档" : "恢复为活动会话"}。</p>,
      confirmLabel: deleting ? "永久删除" : action === "archive" ? "归档" : "恢复",
      tone: deleting ? "danger" : "warning",
      onConfirm: async () => {
        setSessionMutating(true);
        setSessionActionMessage(null);
        try {
          const next = await invoke<ScanReport>("mutate_thread", { codexHome: codexHome.trim(), threadId: thread.threadId, action, confirmedCodexClosed: true });
          setReport(next);
          setSessionActionMessage(deleting ? "会话已永久删除。" : action === "archive" ? "会话已归档。" : "会话已恢复。" );
        } catch (reason) {
          setError(String(reason));
        } finally {
          setSessionMutating(false);
        }
      },
    });
  };

  const openThread = async (thread: ThreadBundle, page = 1) => {
    setSelectedThread(thread);
    setThreadMessagesLoading(true);
    try {
      const result = await invoke<ThreadMessagesPage>("get_thread_messages", { codexHome: codexHome.trim(), threadId: thread.threadId, page, pageSize: 50 });
      setThreadMessages(result);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setThreadMessagesLoading(false);
    }
  };

  const requestProjectAction = (threads: ThreadBundle[], action: "archive" | "restore" | "delete") => {
    if (threads.length === 0) return;
    const deleting = action === "delete";
    setConfirmation({
      title: deleting ? "物理删除项目" : action === "archive" ? "归档项目" : "恢复项目",
      description: deleting
        ? <p>该项目下的 {threads.length} 个会话及其 rollout、数据库记录将被永久删除，无法恢复。</p>
        : <p>将该项目下的 {threads.length} 个会话{action === "archive" ? "全部移入归档" : "全部恢复为活动会话"}。</p>,
      confirmLabel: deleting ? "永久删除项目" : action === "archive" ? "归档项目" : "恢复项目",
      tone: deleting ? "danger" : "warning",
      onConfirm: async () => {
        setSessionMutating(true);
        setSessionActionMessage(null);
        try {
          let next: ScanReport | null = null;
          for (const thread of threads) {
            next = await invoke<ScanReport>("mutate_thread", { codexHome: codexHome.trim(), threadId: thread.threadId, action, confirmedCodexClosed: true });
          }
          if (next) {
            setReport(next);
            setSessionActionMessage(deleting ? `已永久删除 ${threads.length} 个会话。` : action === "archive" ? `已归档 ${threads.length} 个会话。` : `已恢复 ${threads.length} 个会话。`);
          }
        } catch (reason) {
          setError(String(reason));
        } finally {
          setSessionMutating(false);
        }
      },
    });
  };

  const requestCodexSidebarRepair = () => {
    setConfirmation({
      title: "修复 Codex 侧栏索引",
      description: <p>会清空 Codex 可自动重建的本机侧栏目录，并移除所有没有活动或归档会话的项目定义；不会删除会话、项目目录或远端数据。请确认已完全退出 Codex。</p>,
      confirmLabel: "修复侧栏索引",
      tone: "warning",
      onConfirm: async () => {
        setSessionMutating(true);
        setSessionActionMessage(null);
        try {
          const next = await invoke<ScanReport>("repair_codex_sidebar", { codexHome: codexHome.trim(), confirmedCodexClosed: true });
          setReport(next);
          setSessionActionMessage("已清空侧栏索引并移除空项目定义；重新打开 Codex 后将显示真实会话。");
        } catch (reason) {
          setError(String(reason));
        } finally {
          setSessionMutating(false);
        }
      },
    });
  };

  const sessionReportPanel = report ? <>
    {sessionMutating ? <div className="inline-alert session-action-result" role="status"><RefreshCw size={17} /><span>正在更新会话，请勿重复操作…</span></div> : sessionActionMessage && <div className="inline-alert success session-action-result"><Check size={17} /><span>{sessionActionMessage}</span></div>}
    <div className="button-row"><button className="button secondary small" onClick={requestCodexSidebarRepair} disabled={busy || sessionMutating || !canWrite} title={writeBlockedReason ?? undefined}>修复 Codex 侧栏索引</button></div>
    <section className="metric-grid">
      <article className="metric"><span>活动会话</span><strong>{report.activeCount}</strong></article>
      <article className="metric"><span>已归档</span><strong>{report.archivedCount}</strong></article>
      <article className="metric"><span>Rollout 大小</span><strong>{formatBytes(report.totalRolloutBytes)}</strong></article>
      <article className="metric"><span>扫描警告</span><strong>{report.warnings.length}</strong></article>
    </section>
    <section className={`sessions-workspace ${sessionTab === "compatibility" ? "show-compatibility" : "show-sessions"}`}>
      <div className="session-tab-rail" role="tablist" aria-label="会话与兼容性"><button type="button" role="tab" aria-selected={sessionTab === "sessions"} className={`session-tab-button ${sessionTab === "sessions" ? "selected" : ""}`} onClick={() => setSessionTab("sessions")}>会话浏览</button><button type="button" role="tab" aria-selected={sessionTab === "compatibility"} className={`session-tab-button ${sessionTab === "compatibility" ? "selected" : ""}`} onClick={() => setSessionTab("compatibility")}>兼容性状态{report.warnings.length > 0 && <b>{report.warnings.length}</b>}</button></div>
      <SessionFolderBrowser report={report} onAction={requestSessionAction} onProjectAction={requestProjectAction} onOpen={(thread) => void openThread(thread)} mutating={sessionMutating} />
      <article className="surface session-browser"><div className="section-title"><div><h3>会话浏览</h3><p>活动 / 归档 → 项目 / 无项目 → 会话</p></div><span>{report.threads.length} 个会话</span></div><div className="session-tree">{(["active", "archived"] as const).map((status) => { const statusThreads = recentThreads.filter((thread) => thread.archived === (status === "archived")); const projects = new Map<string, typeof statusThreads>(); statusThreads.forEach((thread) => { const key = thread.workspace.logicalId || thread.workspace.sourcePath || "无项目"; projects.set(key, [...(projects.get(key) ?? []), thread]); }); return <section className="session-level-one" key={status}><div className="session-level-title"><strong>{status === "active" ? "活动" : "归档"}</strong><span>{report.threads.filter((thread) => thread.archived === (status === "archived")).length}</span></div>{[...projects.entries()].map(([project, threads]) => <div className="session-project" key={project}><div className="session-level-two"><span>{project === "无项目" ? "无项目" : "项目"}</span><strong title={project}>{project}</strong><small>{threads.length}</small></div><div className="thread-list">{threads.map((thread) => <div className="thread-row" key={thread.threadId}><div><strong>{thread.title || "未命名会话"}</strong><span title={thread.workspace.sourcePath ?? undefined}>{thread.workspace.sourcePath ?? "未记录工作目录"}</span><small>{thread.modelProvider ?? "unknown"}</small></div><div className="thread-actions"><button className="button secondary small" onClick={() => setConfirmation({ title: status === "active" ? "归档会话" : "恢复会话", description: <p>将“{thread.title || thread.threadId}”{status === "active" ? "移入归档" : "恢复为活动会话"}。</p>, confirmLabel: status === "active" ? "归档" : "恢复", onConfirm: async () => { try { const next = await invoke<ScanReport>("mutate_thread", { codexHome: codexHome.trim(), threadId: thread.threadId, action: status === "active" ? "archive" : "restore", confirmedCodexClosed: true }); setReport(next); } catch (reason) { setError(String(reason)); } } })}>{status === "active" ? "归档" : "恢复"}</button><button className="button danger small" onClick={() => setConfirmation({ title: "物理删除会话", description: <p>会话及其本地 rollout、数据库记录将被永久删除，无法恢复。请确认已完全退出 Codex。</p>, confirmLabel: "永久删除", tone: "danger", onConfirm: async () => { try { const next = await invoke<ScanReport>("mutate_thread", { codexHome: codexHome.trim(), threadId: thread.threadId, action: "delete", confirmedCodexClosed: true }); setReport(next); } catch (reason) { setError(String(reason)); } } })}>删除</button></div></div>)}</div></div>)}</section>; })}</div><div className="session-pagination" aria-label="会话分页"><button className="button secondary small" onClick={() => setSessionPage((page) => Math.max(1, page - 1))} disabled={sessionPage <= 1}>上一页</button><span>第 {sessionPage} / {sessionPageCount} 页 · 每页 {sessionPageSize} 条</span><button className="button secondary small" onClick={() => setSessionPage((page) => Math.min(sessionPageCount, page + 1))} disabled={sessionPage >= sessionPageCount}>下一页</button></div>{recentThreads.length === 0 && <p className="muted-copy">扫描结果没有返回可预览的会话。</p>}</article>
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

  const providerSyncSettings = <section className="surface settings-card provider-sync-settings">
    <div className="section-title"><div><h3>本地会话同步</h3><p>将现有本机会话切换到 config.toml 当前配置的 provider，不访问服务器。</p></div><KeyRound size={20} /></div>
    <div className="provider-sync-scope" aria-label="Provider 同步范围"><span>同步范围：</span><b>活动会话</b><b>归档会话</b></div>
    <div className="button-row">
      <button className="button secondary" onClick={() => void previewProviderSync()} disabled={busy || !codexHome.trim() || !repositoryRoot.trim()}><RefreshCw size={16} />{providerPreviewActive ? "预览中…" : "预览"}</button>
      <button className="button warning" onClick={() => setConfirmation({ title: "同步本机会话 Provider", description: !providerSyncPreview ? <p>执行阶段会先扫描本机会话，再将需要修改的记录同步到 config.toml 当前配置的 Provider；如果已经一致，任务将以 0 条改变完成。任务取消或失败后可以重新执行。请确认 Codex 已完全退出。</p> : providerSyncPreview.noChanges ? <p>当前预览没有发现需要修改的记录。执行时会重新扫描；如果 Provider 仍然一致，任务将以 0 条改变完成。请确认 Codex 已完全退出。</p> : <p>当前预览发现 {providerSyncPreview.rolloutCount} 个 rollout 和 {providerSyncPreview.databaseRowCount} 条数据库记录需要修改。执行时会重新扫描；任务取消或失败后可以再次同步剩余记录。请确认 Codex 已完全退出。</p>, confirmLabel: "开始同步", tone: "warning", onConfirm: () => start("start_provider_sync_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={!canWrite || busy || !codexHome.trim() || !repositoryRoot.trim()}>开始同步</button>
    </div>
    {providerSyncPreview && <div className={`inline-alert ${providerSyncPreview.noChanges ? "success" : "warning"}`}>{providerSyncPreview.noChanges ? <Check size={17} /> : <AlertTriangle size={17} />}<span>{providerSyncPreview.noChanges ? `当前 provider 为 ${providerSyncPreview.provider}，无需同步` : `目标 ${providerSyncPreview.provider} · ${providerSyncPreview.rolloutCount} 个 rollout（${formatBytes(providerSyncPreview.rolloutBytes)}）· ${providerSyncPreview.databaseRowCount} 条 SQLite 记录`}</span></div>}
    {providerSyncPreview && providerSyncPreview.warnings.length > 0 && <div className="inline-alert warning"><AlertTriangle size={17} /><span>扫描发现 {providerSyncPreview.warnings.length} 条警告；对应文件会保持原样并跳过。</span></div>}
    {providerSyncReport && <div className="inline-alert success"><Check size={17} /><span>{providerSyncReport.rolloutCount === 0 && providerSyncReport.databaseRowCount === 0 ? `检查完成：Provider 已是 ${providerSyncReport.provider}，0 条改变` : `已同步到 ${providerSyncReport.provider}：${providerSyncReport.rolloutCount} 个 rollout、${providerSyncReport.databaseRowCount} 条 SQLite 记录发生改变`}</span></div>}
  </section>;

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
    <div className="section-title"><div><h3>本地快照与恢复</h3><p>用于诊断、手动导入和未完成操作恢复；所有写入仍遵守 Codex 关闭检查。</p></div><button className="button primary" onClick={() => void (selectedRemoteId && selectedNamespaceId ? openStaging("snapshot") : start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }))} disabled={busy || !canWrite}>{selectedRemoteId && selectedNamespaceId ? "选择会话创建快照" : "创建完整本地快照"}</button></div>
    <div className="field"><label htmlFor="manifest-path-new">快照清单路径</label><input id="manifest-path-new" value={manifestPath} onChange={(event) => setManifestPath(event.target.value)} placeholder="~/.codex-session-sync/snapshots/<id>.json" /></div>
    <div className="button-row"><button className="button secondary" onClick={() => void start("start_validation_job", { manifestPath: manifestPath.trim(), repositoryRoot: repositoryRoot.trim() })} disabled={busy || !manifestPath.trim() || !isTauriRuntime}>验证快照</button><button className="button danger" onClick={() => setConfirmation({ title: "增量导入快照", description: <p>导入会先备份当前会话，并在校验失败时自动回滚。请确认 Codex 已完全退出。</p>, confirmLabel: "确认备份并导入", tone: "danger", onConfirm: () => start("start_import_job", { manifestPath: manifestPath.trim(), codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={busy || !manifestPath.trim() || !canWrite}>增量导入</button></div>
    <div className="divider" />
    <div className="recovery-row"><div className="field"><label htmlFor="journal-path-new">未完成操作的 Journal 路径</label><div className="path-picker-row"><input id="journal-path-new" value={journalPath} onChange={(event) => setJournalPath(event.target.value)} placeholder="选择 checkout-*.json" /><button type="button" className="button secondary" onClick={() => void selectJournalFile()} disabled={busy || !isTauriRuntime}>选择文件</button></div></div><button className="button warning" onClick={() => setConfirmation({ title: "从备份恢复", description: <p>恢复会根据 Journal 校验当前状态并还原备份。请确认 Codex 已完全退出。</p>, confirmLabel: "确认恢复", tone: "warning", onConfirm: () => start("start_recovery_job", { journalPath: journalPath.trim(), confirmedCodexClosed: true }) })} disabled={busy || !journalPath.trim() || !canWrite}>从备份恢复</button></div>
    {(snapshot || validation || importReport || recoveredJournal) && <div className="result-summary-grid tool-results">{snapshot && <article><span>最新快照</span><strong>{snapshot.threadCount} 个会话</strong><small>{formatBytes(snapshot.totalBytes)} · {snapshot.objectCount} 个对象</small></article>}{validation && <article><span>验证结果</span><strong>{validation.valid ? "完整有效" : "验证失败"}</strong><small>{validation.snapshotId}</small></article>}{importReport && <article><span>导入完成</span><strong>{importReport.importedCount} 新增 / {importReport.skippedCount} 跳过</strong><small title={importReport.backupDir}>已创建备份</small></article>}{recoveredJournal && <article><span>恢复结果</span><strong>{recoveredJournal.status}</strong><small>{recoveredJournal.error ?? recoveredJournal.operationId}</small></article>}</div>}
  </section>;

  const namespacesPanel = <div className="namespaces-manager">
    <div className="section-title"><div><h3>命名空间</h3><p>每个命名空间拥有独立历史，可重命名但稳定 ID 不变。</p></div><button className="button secondary small" onClick={() => void refreshNamespaces()} disabled={busy || !selectedRemoteId}><RefreshCw size={15} />刷新</button></div>
    {selectedRemoteId ? <><section className="namespace-list">{namespaces.map((namespace) => { const active = namespaceStatus?.activeNamespaceId === namespace.id; const selected = selectedNamespaceId === namespace.id; return <article key={namespace.id} className={`surface namespace-list-card ${selected ? "selected" : ""}`}><div><div className="namespace-name"><h3>{namespace.displayName}</h3>{active && <StatusBadge tone="success">当前活动</StatusBadge>}</div><CopyCode value={namespace.head ?? "空命名空间"} compact /></div><div className="namespace-card-actions"><button className="button secondary small" onClick={() => void chooseNamespace(namespace.id)} disabled={busy}>{selected ? "已选为目标" : "设为同步目标"}</button></div></article>; })}{namespaces.length === 0 && <section className="surface empty-card"><Database size={28} /><h3>服务器上还没有命名空间</h3><p>创建第一个命名空间后即可推送本机会话。</p></section>}</section><section className="surface namespace-editor-card"><div><h3>{selectedNamespace ? "重命名选中项" : "创建命名空间"}</h3><p>名称可以随时修改，不影响同步身份。</p></div><div className="namespace-editor"><div className="field"><label htmlFor="namespace-name-new">命名空间名称</label><input id="namespace-name-new" value={namespaceName} onChange={(event) => setNamespaceName(event.target.value)} placeholder="例如：工作会话" /></div><div className="button-row"><button className="button primary" onClick={() => void createNamespace()} disabled={busy || !namespaceName.trim()}><Plus size={16} />创建</button><button className="button secondary" onClick={() => void renameNamespace()} disabled={busy || !selectedNamespaceId || !namespaceName.trim()}>保存新名称</button></div></div></section></> : <section className="surface empty-card"><Server size={28} /><h3>请先配置远端服务器</h3><p>命名空间存储在远端服务器中。</p><button className="button primary" onClick={() => navigate("/settings")}>前往设置</button></section>}
  </div>;

  const advancedTools = <details className="surface advanced-fold" open={advancedOpen}>
    <summary onClick={(event) => { event.preventDefault(); setAdvancedOpen(!advancedOpen); }}>高级工具</summary>
    <div className="advanced-section"><h3>自动命名空间选择</h3>{automaticTools}</div>
    <div className="advanced-section"><h3>项目路径</h3>{projectTools}</div>
    <div className="advanced-section"><h3>本地会话同步（Provider）</h3>{providerSyncSettings}</div>
    <div className="advanced-section"><h3>手动快照工具</h3>{snapshotTools}</div>
  </details>;

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
  const allVersionRows: VersionRow[] = [
    ...localVersionRows,
    ...remoteVersionRows,
  ].sort((left, right) => right.createdAt.localeCompare(left.createdAt));
  const syncVersionRows = allVersionRows;
  const selectedLocalSnapshot = localSnapshots.find((item) => item.snapshotId === selectedHistoryId) ?? null;
  const selectedRemoteRevision = remoteRevisions.find((item) => item.revisionId === selectedHistoryId) ?? null;
  const backupCategories: LocalBackupItem["category"][] = ["checkout", "import", "workspace_cleanup", "provider_sync", "other"];
  const historyPage = <div className="page-stack history-page">
    <PageIntro title="快照与恢复" description="以版本图方式浏览本地快照和远端命名空间历史；删除先进入回收站，永久删除或清空回收站时释放空间。" action={<div className="button-row"><button className="button secondary" onClick={() => void refreshHistory()} disabled={historyLoading}><RefreshCw size={15} />刷新</button><button className="button primary" onClick={() => void start("start_snapshot_job", { codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true })} disabled={busy || !canWrite}>创建快照</button></div>} />
    <section className="history-workbench surface">
      <aside className="history-tree">
        <strong>来源</strong>
        <button className={historySource === "all" ? "active" : ""} onClick={() => { setHistorySource("all"); setSelectedHistoryId(null); }}><ArchiveRestore size={15} />所有 <b>{allVersionRows.length}</b></button>
        <button className={historySource === "local" ? "active" : ""} onClick={() => { setHistorySource("local"); setSelectedHistoryId(null); }}><Database size={15} />本机 <b>{localSnapshots.length}</b></button>
        <button className={historySource === "remote" ? "active" : ""} onClick={() => { setHistorySource("remote"); setSelectedHistoryId(null); }} disabled={!selectedNamespaceId}><Server size={15} />{selectedNamespace?.displayName ?? "远端命名空间"} <b>{remoteRevisions.length}</b></button>
        <button className={historySource === "recovery" ? "active" : ""} onClick={() => { setHistorySource("recovery"); setSelectedHistoryId(null); }}><RotateCcw size={15} />操作恢复 <b>{recoveryPoints.filter((point) => point.requiresAttention).length}</b></button>
        <button className={historySource === "trash" ? "active" : ""} onClick={() => { setHistorySource("trash"); setSelectedHistoryId(null); }}><Trash2 size={15} />回收站 <b>{snapshotTrash.length + remoteHistoryTrash.filter((entry) => entry.state === "active").length}</b></button>
        <button className={historySource === "backup" ? "active" : ""} onClick={() => { setHistorySource("backup"); setSelectedHistoryId(null); }}><ArchiveRestore size={15} />备份 <b>{localBackups.length}</b></button>
      </aside>
      <div className="history-main">
        <div className="history-toolbar"><strong>{historySource === "all" ? "全部版本图谱" : historySource === "local" ? "本地快照" : historySource === "remote" ? "远端 Revision" : historySource === "recovery" ? "操作恢复点" : historySource === "trash" ? "可恢复删除" : "本地备份"}</strong>{historySource === "trash" && !historyLoading ? <div className="button-row"><button className="button danger small" onClick={() => void requestLocalTrashPurge([], true)} disabled={snapshotTrash.length === 0}>清空本地</button><button className="button danger small" onClick={() => requestRemoteTrashPurge([], true)} disabled={!selectedNamespaceId || remoteHistoryTrash.every((entry) => entry.state !== "active")}>清空远端</button></div> : historySource === "backup" && !historyLoading ? <div className="button-row"><button className="button secondary small" onClick={() => setSelectedBackupIds(new Set(localBackups.filter((backup) => backup.deletable).map((backup) => backup.id)))} disabled={localBackups.every((backup) => !backup.deletable)}>全选可删除</button><button className="button danger small" onClick={requestBackupDeletion} disabled={selectedBackupIds.size === 0}>删除选中</button></div> : <span>{historyLoading ? historyLoadingLabel : "按创建时间倒序"}</span>}</div>
        {historySource === "all" && <VersionGraphTable rows={allVersionRows} loadingLabel={historyLoading ? historyLoadingLabel : null} selectedId={selectedHistoryId} onSelect={(row) => setSelectedHistoryId(row.id)} />}
        {historySource === "local" && <VersionGraphTable rows={localVersionRows} loadingLabel={historyLoading ? historyLoadingLabel : null} selectedId={selectedHistoryId} onSelect={(row) => setSelectedHistoryId(row.id)} />}
        {historySource === "remote" && <VersionGraphTable rows={remoteVersionRows} loadingLabel={historyLoading ? historyLoadingLabel : null} selectedId={selectedHistoryId} onSelect={(row) => setSelectedHistoryId(row.id)} />}
        {historySource === "recovery" && (historyLoading ? <HistoryLoading label={historyLoadingLabel} /> : <div className="trash-list">{recoveryPoints.map((point) => <article key={point.operationId} className={point.requiresAttention ? "requires-attention" : ""}><div><strong>{point.kind === "checkout" ? "语义切换" : point.kind === "provider_sync" ? "Provider 同步" : "增量导入"} · {point.status}</strong><span>{point.updatedAt ? new Date(point.updatedAt).toLocaleString("zh-CN") : point.journalPath}</span></div>{point.requiresAttention && <button className="button warning small" onClick={() => { setJournalPath(point.journalPath); setConfirmation({ title: "恢复未完成操作", description: <p>将根据 Journal 和备份重新校验后恢复。Codex 必须完全退出。</p>, confirmLabel: "确认恢复", tone: "warning", onConfirm: () => start("start_recovery_job", { journalPath: point.journalPath, confirmedCodexClosed: true }) }); }} disabled={!canWrite || busy}><RotateCcw size={14} />处理恢复</button>}</article>)}{recoveryPoints.length === 0 && <div className="version-log-empty">没有发现操作恢复点</div>}</div>)}
        {historySource === "trash" && (historyLoading ? <HistoryLoading label={historyLoadingLabel} /> : <div className="trash-list">{snapshotTrash.map((entry) => <article key={entry.operationId}><div><strong>本地快照 · {shortHead(entry.snapshotId)}</strong><span>{new Date(entry.trashedAt).toLocaleString("zh-CN")}</span></div><div className="trash-item-actions"><button className="button secondary small" onClick={() => void restoreTrash(entry)}><RotateCcw size={14} />恢复快照</button><button className="button danger small" onClick={() => void requestLocalTrashPurge([entry.operationId], false)}><Trash2 size={14} />永久删除</button></div></article>)}{remoteHistoryTrash.filter((entry) => entry.state === "active").map((entry) => <article key={entry.operationId}><div><strong>远端历史 · {entry.revisionCount} 个版本</strong><span>{shortHead(entry.oldHead)} → {shortHead(entry.newHead)} · 到期 {new Date(entry.expiresAt).toLocaleDateString("zh-CN")}</span></div><div className="trash-item-actions"><button className="button secondary small" onClick={() => void restoreRemoteHistoryTrash(entry)}><RotateCcw size={14} />恢复远端历史</button><button className="button danger small" onClick={() => requestRemoteTrashPurge([entry.operationId], false)}><Trash2 size={14} />永久删除</button></div></article>)}{snapshotTrash.length === 0 && remoteHistoryTrash.every((entry) => entry.state !== "active") && <div className="version-log-empty">回收站为空</div>}</div>)}
        {historySource === "backup" && (historyLoading ? <HistoryLoading label={historyLoadingLabel} /> : <div className="backup-groups">{backupCategories.map((category) => { const entries = localBackups.filter((backup) => backup.category === category); if (entries.length === 0) return null; return <section className="backup-group" key={category}><header><strong>{backupCategoryLabel(category)}</strong><span>{entries.length} 项 · {formatBytes(entries.reduce((total, backup) => total + backup.byteCount, 0))}</span></header><div className="backup-list">{entries.map((backup) => <label className={`backup-item ${backup.deletable ? "" : "protected"}`} key={backup.id}><input type="checkbox" checked={selectedBackupIds.has(backup.id)} onChange={() => toggleBackupSelection(backup.id)} disabled={!backup.deletable || busy} /><div><strong>{backup.path.split(/[\\/]/).pop() ?? backup.id}</strong><span title={backup.path}>{backup.path}</span><small>{backup.location === "repository" ? "同步仓库" : "Codex Home"} · {backup.fileCount} 个文件 · {formatBytes(backup.byteCount)} · {backup.createdAt ? new Date(backup.createdAt).toLocaleString("zh-CN") : "时间未知"}{!backup.deletable ? " · 未完成操作正在使用" : ""}</small></div></label>)}</div></section>; })}{localBackups.length === 0 && <div className="version-log-empty">没有发现本地备份</div>}</div>)}
        {(selectedLocalSnapshot || selectedRemoteRevision) && <section className="version-details">
          <div><span className="overline">选中版本</span><h3>{selectedLocalSnapshot ? (selectedLocalSnapshot.metadata.description || "本地快照") : "远端 Revision"}</h3><CopyCode value={selectedHistoryId ?? ""} /></div>
          <div className="version-detail-metrics"><span>会话 <b>{selectedLocalSnapshot?.threadCount ?? selectedRemoteRevision?.threadCount}</b></span><span>逻辑大小 <b>{formatBytes(selectedLocalSnapshot?.logicalBytes ?? selectedRemoteRevision?.logicalBytes ?? 0)}</b></span><span>物理引用 <b>{formatBytes(selectedLocalSnapshot?.physicalReferencedBytes ?? selectedRemoteRevision?.physicalReferencedBytes ?? 0)}</b></span></div>
          {selectedLocalSnapshot && <div className="button-row"><button className="button warning" onClick={() => setConfirmation({ title: selectedLocalSnapshot.metadata.scope === "selection" ? "精确恢复选择快照" : "语义恢复本地快照", description: selectedLocalSnapshot.metadata.scope === "selection" ? <p>当前快照只包含被勾选的变化。精确恢复会以该集合替换本地会话，未包含的会话将丢失；操作仍会先完整备份并可在失败时回滚。</p> : <p>当前 Codex 会话将先完整备份并写入 Journal，再按线程语义恢复所选快照；Provider、工作区路径和 rollout 换行格式会按当前机器物化。失败时会自动回滚。</p>, confirmLabel: "备份并恢复", tone: "warning", onConfirm: () => start("start_snapshot_restore_job", { manifestPath: selectedLocalSnapshot.manifestPath, codexHome: codexHome.trim(), repositoryRoot: repositoryRoot.trim(), confirmedCodexClosed: true }) })} disabled={!canWrite || busy}>{selectedLocalSnapshot.metadata.scope === "selection" ? "精确恢复（会替换）" : "语义恢复"}</button><button className="button secondary" onClick={async () => { setHistoryLoading(true); setHistoryLoadingLabel("正在更新快照标记…"); try { await invoke("update_snapshot_metadata", { repositoryRoot: repositoryRoot.trim(), snapshotId: selectedLocalSnapshot.snapshotId, metadata: { ...selectedLocalSnapshot.metadata, pinned: !selectedLocalSnapshot.metadata.pinned } }); await refreshHistory("正在更新快照列表…"); } catch (reason) { setError(String(reason)); } finally { setHistoryLoading(false); } }}>{selectedLocalSnapshot.metadata.pinned ? "取消固定" : "固定快照"}</button><button className="button danger" onClick={() => void requestSnapshotTrash(selectedLocalSnapshot)} disabled={selectedLocalSnapshot.metadata.pinned || historyLoading}><Trash2 size={15} />移入回收站</button></div>}
          {selectedRemoteRevision && <div className="button-row"><button className="button secondary" onClick={() => void start("start_remote_revision_download_job", { repositoryRoot: repositoryRoot.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId })} disabled={busy}>下载为本地快照</button><button className="button secondary" onClick={() => setConfirmation({ title: "恢复为本地待推送状态", description: <p>当前会话会先备份并通过 Journal 精确切换到该远端版本；Tracking 仍保留当前远端 Head，之后可普通 Push 发布。</p>, confirmLabel: "备份并恢复", tone: "warning", onConfirm: () => start("start_remote_revision_restore_job", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId, publish: false, confirmedCodexClosed: true }) })} disabled={busy || !canWrite}>恢复为待 Push</button><button className="button warning" onClick={() => setConfirmation({ title: "恢复并发布为新版本", description: <p>先安全恢复所选历史内容，再以当前远端 Head 为父版本发布新的 Revision；不会改写已有历史。</p>, confirmLabel: "恢复并发布", tone: "warning", onConfirm: () => start("start_remote_revision_restore_job", { repositoryRoot: repositoryRoot.trim(), codexHome: codexHome.trim(), remoteId: selectedRemoteId, namespaceId: selectedNamespaceId, revisionId: selectedRemoteRevision.revisionId, publish: true, confirmedCodexClosed: true }) })} disabled={busy || !canWrite}>恢复并发布</button>{remoteRevisions[0]?.revisionId !== selectedRemoteRevision.revisionId && <button className="button danger" onClick={() => setConfirmation({ title: "回退远端 Head", description: <p>该版本之后的远端历史会进入 30 天可恢复回收站，Namespace Epoch 将递增。对象不会立即删除。</p>, confirmLabel: "确认回退 Head", tone: "danger", onConfirm: () => void truncateRemoteHistory(selectedRemoteRevision.revisionId, "正在回退远端 Head…") })}>回退 Head 到此处</button>}{remoteRevisions[0]?.revisionId === selectedRemoteRevision.revisionId && <button className="button danger" onClick={() => setConfirmation({ title: "删除当前远端 Head", description: <p>当前 Head 会进入 30 天可恢复回收站，父版本成为新 Head；共享对象和内容不会立即删除。</p>, confirmLabel: "删除当前 Head", tone: "danger", onConfirm: () => void truncateRemoteHistory(selectedRemoteRevision.parentRevision, "正在删除远端 Head…") })}>删除当前 Head</button>}</div>}
        </section>}
      </div>
    </section>
    {storageSummary && <section className="surface storage-summary" aria-label="仓库存储统计"><div><span>仓库占用</span><strong>{formatBytes(storageSummary.repositoryPhysicalBytes)}</strong></div><div><span>活动可达</span><strong>{formatBytes(storageSummary.activePhysicalBytes)}</strong></div><div><span>共享对象</span><strong>{formatBytes(storageSummary.sharedPhysicalBytes)}</strong></div><div><span>回收站保护</span><strong>{formatBytes(storageSummary.trashBytes)}</strong></div><div><span>可释放</span><strong>{formatBytes(storageSummary.reclaimableBytes)}</strong></div><div><span>待清理</span><strong>{formatBytes(storageSummary.gcQuarantineBytes)}</strong></div></section>}
  </div>;

  return <AppShell processes={processes} busy={busy} onRefreshProcesses={() => void refreshProcesses()} updateAvailable={Boolean(availableUpdate)} onOpenUpdates={openUpdates}>
    {processes.length > 0 && <div className="global-process-alert" role="status"><AlertTriangle size={18} /><div><strong>检测到 Codex 正在运行</strong><span>扫描和配置仍可使用；同步、导入、恢复和清理暂时禁用。</span><div className="process-chips">{processes.map((process) => <code key={process.pid}>{process.kind} · {process.name} · PID {process.pid}</code>)}</div></div></div>}
    {location.pathname === "/settings" && <div className="settings-tabbar surface" role="tablist" aria-label="设置分类"><button type="button" role="tab" aria-selected={settingsTab === "local"} className={settingsTab === "local" ? "active" : ""} onClick={() => setSettingsTab("local")}>本机与外观</button><button type="button" role="tab" aria-selected={settingsTab === "remote"} className={settingsTab === "remote" ? "active" : ""} onClick={() => setSettingsTab("remote")}>远端与命名空间</button><button type="button" role="tab" aria-selected={settingsTab === "advanced"} className={settingsTab === "advanced" ? "active" : ""} onClick={() => { setSettingsTab("advanced"); setAdvancedOpen(true); }}>自动选择</button><button type="button" role="tab" aria-selected={settingsTab === "project"} className={settingsTab === "project" ? "active" : ""} onClick={() => { setSettingsTab("project"); setAdvancedOpen(true); }}>项目路径</button><button type="button" role="tab" aria-selected={settingsTab === "provider"} className={settingsTab === "provider" ? "active" : ""} onClick={() => { setSettingsTab("provider"); setAdvancedOpen(true); }}>Provider 同步</button><button type="button" role="tab" aria-selected={settingsTab === "snapshots"} className={settingsTab === "snapshots" ? "active" : ""} onClick={() => { setSettingsTab("snapshots"); setAdvancedOpen(true); }}>快照工具</button></div>}
    <Routes>
      <Route path="/" element={<Navigate to="/overview" replace />} />
      <Route path="/overview" element={<div className="page-stack">
        <PageIntro title="欢迎使用 Codex Session Sync" description="通过自托管服务器，在多台电脑之间安全同步 Codex 会话。" action={<button className="button primary" onClick={() => navigate(setupComplete ? "/sync" : setupSteps.find((step) => !step.ready)?.route ?? "/sync")}>{setupComplete ? "开始同步" : "继续配置"}<ArrowRight size={16} /></button>} />
        <section className={`surface readiness-card ${setupComplete ? "complete" : ""}`}><div className="readiness-heading"><div><span className="overline">当前状态</span><h3>{setupComplete ? "同步环境已就绪" : "完成以下配置即可开始"}</h3><p>{workflowNextStep}</p></div><StatusBadge tone={setupComplete ? "success" : "warning"}>{setupComplete ? "准备完成" : `${setupSteps.filter((step) => step.ready).length} / ${setupSteps.length}`}</StatusBadge></div><div className="setup-steps">{setupSteps.map((step, index) => <button key={step.label} type="button" onClick={() => navigate(step.route)}><span className={`step-index ${step.ready ? "ready" : ""}`}>{step.ready ? <Check size={15} /> : index + 1}</span><span><strong>{step.label}</strong><small title={step.detail}>{step.detail}</small></span><ChevronRight size={16} /></button>)}</div></section>
        <section className="overview-grid"><article className="surface overview-card"><div className="section-title"><h3>当前同步上下文</h3><button className="text-button" onClick={() => navigate("/sync")}>查看同步</button></div><dl className="summary-list"><div><dt>Codex Home</dt><dd title={codexHome}>{codexHome || "未设置"}</dd></div><div><dt>远端服务器</dt><dd>{selectedProfile?.displayName ?? "未选择"}</dd></div><div><dt>命名空间</dt><dd>{selectedNamespace?.displayName ?? "未选择"}</dd></div></dl></article><article className="surface overview-card"><div className="section-title"><h3>本机会话</h3><button className="text-button" onClick={() => navigate("/sessions")}>查看会话</button></div>{report ? <div className="overview-metrics"><div><strong>{report.totalCount}</strong><span>总会话</span></div><div><strong>{formatBytes(report.totalRolloutBytes)}</strong><span>Rollout</span></div><div><strong>{report.warnings.length}</strong><span>警告</span></div></div> : <p className="muted-copy">尚未扫描本机会话。</p>}</article><article className="surface overview-card"><div className="section-title"><h3>最近同步</h3>{syncReport && <StatusBadge tone={syncOutcomeTone(syncReport.kind)}>{syncOutcomeLabel(syncReport.kind)}</StatusBadge>}</div>{syncReport ? <div className="latest-sync"><strong>{syncReport.threadCount} 个会话</strong><CopyCode value={syncReport.head ?? "无 Head"} compact /><span>↑ {syncReport.uploadedObjects} · ↓ {syncReport.downloadedObjects}</span></div> : <p className="muted-copy">当前运行尚无同步结果。</p>}</article></section>
      </div>} />
      <Route path="/sync" element={<div className="page-stack compact-stack">
        <PageIntro title="同步会话" description="按步骤完成：① 选择同步目标 → ② 选择方向 → ③ 处理冲突 → ④ 需要时查看历史与恢复。" />
        <section className="surface context-selector"><div className="field"><label>Codex Home</label><button className="selector-display" onClick={() => navigate("/settings")} title={codexHome}>{codexHome || "未设置"}<Settings size={15} /></button></div><div className="field"><label htmlFor="sync-remote">远端服务器</label><select id="sync-remote" value={selectedRemoteId} onChange={(event) => setSelectedRemoteId(event.target.value)} disabled={busy}><option value="">请选择远端</option>{profiles.map((profile) => <option key={profile.id} value={profile.id}>{profile.displayName}</option>)}</select></div><div className="field"><label htmlFor="sync-namespace">命名空间</label><select id="sync-namespace" value={selectedNamespaceId} onChange={(event) => void chooseNamespace(event.target.value)} disabled={busy || !selectedRemoteId}><option value="">请选择命名空间</option>{namespaces.map((namespace) => <option key={namespace.id} value={namespace.id}>{namespace.displayName}</option>)}</select>{mappingState && <small>{selectionSourceLabel(mappingState.selection.source)}</small>}</div></section>
        <section className="surface sync-tabs" aria-label="同步工作区">
          <div className="sync-tab-list" role="tablist" aria-label="同步与快照">
            <button type="button" role="tab" aria-selected={!historyOpen} className={!historyOpen ? "active" : ""} onClick={() => setHistoryOpen(false)}>同步操作</button>
            <button type="button" role="tab" aria-selected={historyOpen} className={historyOpen ? "active" : ""} onClick={() => setHistoryOpen(true)}>快照与恢复</button>
          </div>
          {!historyOpen && <div className="sync-tab-panel" role="tabpanel">
            {syncStatusPanel}<section className="surface sync-version-log"><div className="section-title"><div><h3>版本图谱</h3><p>本地快照与当前远端命名空间共享同一种 IDEA 风格版本日志。</p></div><button className="text-button" onClick={() => setHistoryOpen(true)}>打开历史与恢复</button></div><VersionGraphTable rows={syncVersionRows} loadingLabel={historyLoading ? historyLoadingLabel : null} selectedId={selectedHistoryId} onSelect={(row) => { setSelectedHistoryId(row.id); setHistorySource(row.kind); setHistoryOpen(true); }} /></section>{syncResultPanel}
          </div>}
          {historyOpen && <div className="sync-tab-panel" role="tabpanel">{historyPage}</div>}
        </section>
      </div>} />
      <Route path="/sessions" element={<div className="page-stack"><PageIntro title="本机会话" description="扫描会在后台运行，只读取会话和兼容性信息。" action={<button className="button primary" onClick={() => void start("start_scan_job", { codexHome: codexHome.trim() })} disabled={busy || !codexHome.trim() || !isTauriRuntime}><RefreshCw size={16} />重新扫描</button>} />{sessionReportPanel}</div>} />
      <Route path="/settings" element={<div className="page-stack"><PageIntro title="设置" description="配置本机数据位置、远端服务器、命名空间和高级工具。" /><section className="settings-grid"><article className="surface settings-card"><div className="section-title"><div><h3>本机存储</h3><p>路径变化会刷新对应的远端与同步状态。</p></div><Database size={20} /></div><div className="field"><label htmlFor="codex-home-new">Codex Home</label><input id="codex-home-new" value={codexHome} onChange={(event) => setCodexHome(event.target.value)} disabled={busy} /></div><div className="field"><label htmlFor="repository-root-new">本地同步仓库</label><input id="repository-root-new" value={repositoryRoot} onChange={(event) => setRepositoryRoot(event.target.value)} disabled={busy} /></div></article><article className="surface settings-card"><div className="section-title"><div><h3>外观</h3><p>默认跟随操作系统，也可以固定主题。</p></div>{resolvedTheme === "dark" ? <Moon size={20} /> : <Sun size={20} />}</div><div className="theme-options" role="radiogroup" aria-label="主题"><button role="radio" aria-checked={themePreference === "system"} className={themePreference === "system" ? "selected" : ""} onClick={() => setThemePreference("system")}><RefreshCw size={17} /><span><strong>跟随系统</strong><small>当前为{resolvedTheme === "dark" ? "深色" : "浅色"}</small></span></button><button role="radio" aria-checked={themePreference === "light"} className={themePreference === "light" ? "selected" : ""} onClick={() => setThemePreference("light")}><Sun size={17} /><span><strong>浅色</strong><small>始终使用浅色界面</small></span></button><button role="radio" aria-checked={themePreference === "dark"} className={themePreference === "dark" ? "selected" : ""} onClick={() => setThemePreference("dark")}><Moon size={17} /><span><strong>深色</strong><small>始终使用深色界面</small></span></button></div></article></section><section className="surface settings-card remote-settings"><div className="section-title"><div><h3>远端服务器</h3><p>Bearer Token 只保存到操作系统凭据库，前端不会读回明文。</p></div><StatusBadge>{profiles.length} 个配置</StatusBadge></div><div className="profile-tabs">{profiles.map((profile) => <button key={profile.id} className={selectedRemoteId === profile.id ? "selected" : ""} onClick={() => setSelectedRemoteId(profile.id)} disabled={busy}>{profile.displayName}</button>)}<button onClick={() => { setSelectedRemoteId(""); setRemoteName("个人服务器"); setRemoteUrl("http://127.0.0.1:8787"); setRemoteToken(""); setNamespaces([]); setSelectedNamespaceId(""); setMappingState(null); setWorkspaceMappingState(null); }} disabled={busy}><Plus size={15} />新建远端</button></div><div className="remote-form"><div className="field"><label htmlFor="remote-name-new">配置名称</label><input id="remote-name-new" value={remoteName} onChange={(event) => setRemoteName(event.target.value)} /></div><div className="field"><label htmlFor="remote-url-new">服务器 URL</label><input id="remote-url-new" value={remoteUrl} onChange={(event) => setRemoteUrl(event.target.value)} /></div><div className="field"><label htmlFor="remote-token-new">Bearer Token</label><input id="remote-token-new" type="password" value={remoteToken} onChange={(event) => setRemoteToken(event.target.value)} placeholder={selectedProfile?.credentialConfigured ? "已保存；留空则不修改" : "至少 16 位可见 ASCII 字符"} /></div></div><div className="button-row"><button className="button primary" onClick={() => void saveRemote()} disabled={busy || !remoteName.trim() || !remoteUrl.trim() || (!selectedRemoteId && !remoteToken.trim())}>保存并验证</button><button className="button secondary" onClick={() => void testConnection()} disabled={busy || !selectedRemoteId}>测试连接</button></div>{(selectedProfile?.insecureHttp || remoteUrl.trim().startsWith("http://")) && <div className="inline-alert warning"><AlertTriangle size={17} /><span>当前连接未使用 HTTPS，仅建议在本机或可信内网使用。</span></div>}{connectionMessage && <div className="inline-alert success"><Check size={17} /><span>{connectionMessage}</span></div>}</section><section className="surface settings-card"><div className="section-title"><div><h3>组织会话</h3><p>创建命名空间来分隔不同电脑或用途的会话集合。</p></div><Settings size={20} /></div>{namespacesPanel}</section>{advancedTools}</div>} />
      <Route path="/me" element={<div className="page-stack my-page"><PageIntro title="我的" description="项目主页、个人资料模板和应用更新。" /><section className="surface my-tabs" role="tablist" aria-label="我的"><button type="button" role="tab" aria-selected={myTab === "home"} className={myTab === "home" ? "active" : ""} onClick={() => setMyTab("home")}><UserRound size={16} />主页</button><button type="button" role="tab" aria-selected={myTab === "updates"} className={myTab === "updates" ? "active" : ""} onClick={() => setMyTab("updates")}><Download size={16} />更新{availableUpdate && <b>1</b>}</button></section>{myTab === "home" ? <section className="my-home-grid"><article className="surface personal-card"><div className="section-title"><div><span className="overline">个人资料模板</span><h3>关于作者</h3><p>请按需要直接修改本页中的占位内容。</p></div><UserRound size={21} /></div><dl className="profile-template"><div><dt>昵称</dt><dd>待填写</dd></div><div><dt>邮箱</dt><dd>待填写@example.com</dd></div><div><dt>个人简介</dt><dd>在这里介绍你自己、项目目标或联系方式。</dd></div><div><dt>其他链接</dt><dd>待填写</dd></div></dl></article><article className="surface project-card"><div className="section-title"><div><span className="overline">开源项目</span><h3>Codex Session Sync</h3><p>跨设备同步 Codex 会话的个人自托管工具。</p></div><Server size={21} /></div><div className="project-address"><span>GitHub 地址</span><CopyCode value={PROJECT_URL} /></div><p className="muted-copy">Release、更新说明和安装包均通过该 GitHub 仓库发布。</p></article></section> : <section className="surface update-card"><div className="section-title"><div><span className="overline">应用更新</span><h3>GitHub Releases</h3><p>更新包会先完成签名验证，验证通过才会安装。</p></div><button type="button" className="button secondary small" onClick={() => void checkForUpdate(true)} disabled={updateChecking || updateInstalling}><RefreshCw size={14} />{updateChecking ? "检查中…" : "手动检查"}</button></div>{updateMessage && !availableUpdate && <div className="inline-alert"><Check size={17} /><span>{updateMessage}</span></div>}<UpdateDetails update={availableUpdate} currentVersion={desktopPackage.version} progress={availableUpdate ? updateMessage : null} onInstall={() => void installUpdate()} installing={updateInstalling} /><div className="project-address update-source"><span>更新源</span><CopyCode value={`${PROJECT_URL}/releases/latest`} /></div></section>}</div>} />
      <Route path="*" element={<Navigate to="/overview" replace />} />
    </Routes>

    {pendingWorkspaceSync && <div className="dialog-backdrop" role="presentation"><section className="workspace-path-modal" role="dialog" aria-modal="true" aria-label="设置本机项目路径"><div className="workspace-modal-heading"><div><span className="overline">同步前路径检查</span><h2>设置本机项目路径</h2></div><button type="button" className="icon-button" onClick={() => setPendingWorkspaceSync(null)} disabled={busy} aria-label="关闭"><X size={19} /></button></div><p>远端会话引用了当前电脑尚不可用的项目路径。选择统一父目录后仍可逐项修改。</p><div className="migration-summary"><strong>{pendingWorkspaceSync.plan.unmappedPaths.length} 项待设置</strong><span>{pendingWorkspaceSync.plan.mappedPathCount} 项已有映射 · {pendingWorkspaceSync.plan.existingPathCount} 项原路径可用</span></div><WorkspacePathEditor parentDirectory={workspaceEditorParent} drafts={workspaceDrafts} busy={busy} submitLabel="保存路径并继续" onParentChange={(value) => changeEditorParent("sync", value)} onTargetChange={(index, value) => setWorkspaceDrafts((current) => current.map((draft, candidate) => candidate === index ? { ...draft, localPath: value } : draft))} onChooseParent={() => void chooseEditorParent("sync")} onChooseTarget={(index) => void chooseEditorTarget("sync", index)} onSubmit={() => void saveWorkspaceDraftsAndContinue()} onCancel={() => setPendingWorkspaceSync(null)} /></section></div>}
    {selectedThread && <div className="dialog-backdrop" role="presentation"><section className="thread-detail-modal" role="dialog" aria-modal="true" aria-label="完整会话"><header><div><span className="overline">会话详情</span><h2>{selectedThread.title || "未命名会话"}</h2><small>{selectedThread.workspace.sourcePath ?? selectedThread.threadId}</small></div><button type="button" className="icon-button" onClick={() => { setSelectedThread(null); setThreadMessages(null); }} aria-label="关闭"><X size={19} /></button></header>{threadMessagesLoading ? <div className="thread-detail-loading"><RefreshCw size={19} /><span>正在读取会话…</span></div> : threadMessages ? <><div className="message-list">{threadMessages.messages.map((message) => <article className={"message-card role-" + message.role} key={message.index}><div><strong>{message.role === "user" ? "用户" : message.role === "assistant" ? "助手" : message.role}</strong><small>{message.timestamp ? new Date(message.timestamp).toLocaleString("zh-CN") : "#" + (message.index + 1)}</small></div><pre>{message.text}</pre></article>)}{threadMessages.messages.length === 0 && <p className="muted-copy">这一页没有可显示的消息。</p>}</div>{threadMessages.warnings.length > 0 && <div className="inline-alert warning"><AlertTriangle size={17} /><span>有 {threadMessages.warnings.length} 行无法解析，其他消息仍已显示。</span></div>}<FolderPager page={threadMessages.page} pageCount={Math.max(1, Math.ceil(threadMessages.totalCount / threadMessages.pageSize))} total={threadMessages.totalCount} onChange={(page) => void openThread(selectedThread, page)} /></> : null}</section></div>}
    <StagingDialog
      plan={stagingPlan}
      selected={stagedThreadIds}
      showAll={stagingShowAll}
      busy={busy}
      actionLabel={stagingAction === "push" ? "推送已暂存变化" : "创建选择快照"}
      onToggle={toggleStaged}
      onSetAll={(checked) => setStagedThreadIds(checked ? new Set((stagingPlan?.candidates ?? []).filter((candidate) => candidate.kind !== "unchanged").map((candidate) => candidate.threadId)) : new Set())}
      onShowAll={setStagingShowAll}
      onClose={() => setStagingPlan(null)}
      onPush={submitStaged}
    />
    <ConfirmDialog request={confirmation} onClose={() => setConfirmation(null)} />
    <ErrorDialog message={error} onClose={() => setError(null)} />
    {updatePromptOpen && <UpdatePrompt update={availableUpdate} currentVersion={desktopPackage.version} onDismiss={dismissUpdatePrompt} onOpenUpdates={openUpdates} onInstall={() => void installUpdate()} installing={updateInstalling} />}
    {job && <aside className={`task-center ${jobFailure ? "failed" : ""}`} aria-live="polite"><div className="task-center-heading"><div><span className="overline">{job.kind} · {job.state}</span><strong>{jobFailure ? "任务失败" : job.progress.phase.replaceAll("_", " ")}</strong></div>{!isActive(job) && <button className="icon-button" onClick={() => setJob(null)} aria-label="关闭任务"><X size={17} /></button>}</div><p>{jobFailure ?? job.progress.message}</p><div className={`progress-track ${progressPercent === null ? "indeterminate" : ""}`}><div className="progress-fill" style={{ width: progressPercent === null ? undefined : `${progressPercent}%` }} /></div><div className="task-center-footer"><small>{progressDetail}</small>{isActive(job) && <button className="button danger small" onClick={() => void cancelCurrentJob()} disabled={!job.cancellable || job.state === "cancelling"}>{job.state === "cancelling" ? "正在安全停止…" : job.cancellable ? "取消任务" : "当前阶段不可取消"}</button>}</div></aside>}
  </AppShell>;

}
