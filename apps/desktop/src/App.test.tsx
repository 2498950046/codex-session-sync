import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "./router";
import { beforeEach, expect, test, vi } from "vitest";
import App from "./App";
import { ThemeProvider } from "./theme";

const { invokeMock, openMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  openMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: openMock }));

const remoteId = "remote-1";
const namespaceId = "namespace-1";
let runningProcesses = false;
let providerPreviewResult = { provider: "openai", rolloutCount: 12, rolloutBytes: 4096, databaseRowCount: 12, noChanges: false, warnings: [] };

function providerPreviewJob(state: "running" | "completed") {
  return {
    jobId: "provider-preview-1",
    kind: "provider_sync_preview",
    state,
    progress: state === "running"
      ? { phase: "scan_rollouts", message: "正在扫描活动与归档会话", completed: 120, total: 418, unit: "rollouts", cancellable: true }
      : { phase: "completed", message: "任务已完成", completed: 1, total: 1, unit: "tasks", cancellable: false },
    cancellable: state === "running",
    resultReady: state === "completed",
    error: null,
  };
}

function response(command: string) {
  if (command === "get_default_codex_home") return "C:/Users/test/.codex";
  if (command === "get_default_repository_root") return "C:/Users/test/.codex-session-sync";
  if (command === "list_codex_processes") return runningProcesses ? [{ pid: 7, name: "Codex", executable: null, commandLine: [], kind: "desktop" }] : [];
  if (command === "list_remote_profiles") return [{ id: remoteId, displayName: "测试服务器", serverUrl: "https://sync.test", selectedNamespaceId: namespaceId, automaticNamespaceSelection: false, createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z", credentialConfigured: true, insecureHttp: false }];
  if (command === "list_remote_namespaces") return [{ id: namespaceId, displayName: "测试会话", head: "sha256:remote", createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z" }];
  if (command === "get_namespace_mapping_state") return { remoteId, automaticEnabled: false, context: { codexHomeKey: "c:/users/test/.codex", provider: "openai", apiKeyAvailable: false, apiKeyFingerprintHint: null, apiKeySource: null, warnings: [] }, mappings: [], selection: { selectedNamespaceId: namespaceId, source: "profile_default", matchedMappingId: null, ambiguousMappingIds: [] } };
  if (command === "get_remote_namespace_status") return { remoteId, namespaceId, active: true, activeRemoteId: remoteId, activeNamespaceId: namespaceId, integratedHead: "sha256:remote", remoteHead: "sha256:remote", generation: 2 };
  if (command === "get_workspace_mapping_state") return { remoteId, namespaceId, codexHomeKey: "c:/users/test/.codex", mappings: [] };
  if (command === "list_local_snapshots") return [{ snapshotId: "01900000-0000-7000-8000-000000000001", createdAt: "2026-07-31T10:00:00Z", manifestPath: "C:/Users/test/.codex-session-sync/snapshots/01900000-0000-7000-8000-000000000001.json", threadCount: 12, objectCount: 20, logicalBytes: 2048, physicalReferencedBytes: 1024, warningCount: 0, metadata: { description: "发布前", tags: ["manual"], pinned: false, automatic: false } }];
  if (command === "list_local_snapshot_trash") return [];
  if (command === "get_repository_storage_summary") return { logicalBytes: 2048, repositoryPhysicalBytes: 1024, activePhysicalBytes: 1024, sharedPhysicalBytes: 512, exclusivePhysicalBytes: 512, trashBytes: 0, gcQuarantineBytes: 0, reclaimableBytes: 0, protectedByJournalBytes: 0 };
  if (command === "list_recovery_points") return [];
  if (command === "list_remote_revisions") return [{ revisionId: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", namespaceId, parentRevision: null, createdAt: "2026-07-31T09:00:00Z", threadCount: 10, objectCount: 18, logicalBytes: 1800, physicalReferencedBytes: 900, state: "active" }];
  if (command === "list_remote_history_trash") return [];
  if (command === "start_provider_sync_preview_job") return providerPreviewJob("running");
  if (command === "get_job") return providerPreviewJob("completed");
  if (command === "take_job_result") return providerPreviewResult;
  if (command === "update_snapshot_metadata") return { description: "发布前", tags: ["manual"], pinned: true, automatic: false };
  if (command === "plan_snapshot_deletion") return { snapshotId: "01900000-0000-7000-8000-000000000001", manifestPath: "C:/Users/test/.codex-session-sync/snapshots/01900000-0000-7000-8000-000000000001.json", pinned: false, sharedObjectCount: 10, exclusiveObjectCount: 10, estimatedReclaimableBytes: 512, planFingerprint: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" };
  if (command === "trash_local_snapshot") return { operationId: "01900000-0000-7000-8000-000000000002", snapshotId: "01900000-0000-7000-8000-000000000001", trashedAt: "2026-07-31T11:00:00Z", originalManifestPath: "snapshot.json", trashManifestPath: "trash.json" };
  throw new Error(`Unexpected command in test: ${command}`);
}

beforeEach(() => {
  runningProcesses = false;
  providerPreviewResult = { provider: "openai", rolloutCount: 12, rolloutBytes: 4096, databaseRowCount: 12, noChanges: false, warnings: [] };
  invokeMock.mockImplementation((command: string) => Promise.resolve(response(command)));
  Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
});

async function expandAdvanced(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByText("高级工具"));
}

test("ready sync context exposes both directions and process detection gates writes", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><MemoryRouter initialEntries={["/sync"]}><App /></MemoryRouter></ThemeProvider>);

  const push = await screen.findByRole("button", { name: "推送" });
  const pull = screen.getByRole("button", { name: "拉取" });
  expect(push).toBeEnabled();
  expect(pull).toBeEnabled();

  runningProcesses = true;
  await user.click(screen.getByRole("button", { name: "Codex 已退出" }));

  await waitFor(() => {
    expect(screen.getByRole("button", { name: /Codex 运行中/ })).toBeInTheDocument();
    expect(push).toBeDisabled();
    expect(pull).toBeDisabled();
  });
});

test("history graph selects snapshots and exposes recoverable local deletion", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><MemoryRouter initialEntries={["/sync"]}><App /></MemoryRouter></ThemeProvider>);

  await user.click(await screen.findByRole("button", { name: "打开历史与恢复" }));
  expect(await screen.findByRole("heading", { level: 2, name: "快照与恢复" })).toBeInTheDocument();
  expect((await screen.findAllByText("发布前")).length).toBeGreaterThan(0);
  expect(screen.getByLabelText("仓库存储统计")).toHaveTextContent("仓库占用");

  await user.click(screen.getAllByText("发布前").at(-1)!);
  expect(await screen.findByRole("button", { name: "语义恢复" })).toBeInTheDocument();
  await user.click(screen.getAllByRole("button", { name: /移入回收站/ }).at(-1)!);
  expect(await screen.findByText("将快照移入回收站")).toBeInTheDocument();
  await user.click(screen.getAllByRole("button", { name: /移入回收站/ }).at(-1)!);

  await waitFor(() => {
    expect(invokeMock).toHaveBeenCalledWith("trash_local_snapshot", expect.any(Object));
  });
});

test("provider preview runs as a progress job and disables the write action", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><MemoryRouter initialEntries={["/settings"]}><App /></MemoryRouter></ThemeProvider>);

  await screen.findByRole("heading", { level: 2, name: "设置" });
  await expandAdvanced(user);
  const preview = await screen.findByRole("button", { name: "预览" });
  expect(screen.getByLabelText("Provider 同步范围")).toHaveTextContent("同步范围：活动会话归档会话");
  await waitFor(() => expect(preview).toBeEnabled());
  await user.click(preview);
  expect(await screen.findByText("provider_sync_preview · running")).toBeInTheDocument();
  expect(screen.getByText("正在扫描活动与归档会话")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "备份并同步" })).toBeDisabled();
  expect(await screen.findByText(/目标 openai/)).toBeInTheDocument();
  expect(await screen.findByRole("button", { name: "备份并同步" })).toBeEnabled();
});

test("provider sync panel now lives inside settings advanced tools", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><MemoryRouter initialEntries={["/settings"]}><App /></MemoryRouter></ThemeProvider>);

  expect(await screen.findByRole("heading", { level: 2, name: "设置" })).toBeInTheDocument();
  const panel = screen.getByText("本地会话同步");
  const advancedDetails = panel.closest("details.advanced-fold");
  expect(advancedDetails).not.toBeNull();
  expect(advancedDetails).not.toHaveAttribute("open");
  await expandAdvanced(user);
  expect(advancedDetails).toHaveAttribute("open");
  expect(panel).toBeVisible();
});

test("provider sync remains available when preview reports zero changes", async () => {
  const user = userEvent.setup();
  providerPreviewResult = { provider: "openai", rolloutCount: 0, rolloutBytes: 0, databaseRowCount: 0, noChanges: true, warnings: [] };
  render(<ThemeProvider><MemoryRouter initialEntries={["/settings"]}><App /></MemoryRouter></ThemeProvider>);

  await screen.findByRole("heading", { level: 2, name: "设置" });
  await expandAdvanced(user);
  const preview = await screen.findByRole("button", { name: "预览" });
  await waitFor(() => expect(preview).toBeEnabled());
  await user.click(preview);
  expect(await screen.findByText("当前 provider 为 openai，无需同步")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "备份并同步" }));
  expect(await screen.findByText(/任务将以 0 条改变完成/)).toBeInTheDocument();
});

test("provider sync is available before running an optional preview", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><MemoryRouter initialEntries={["/settings"]}><App /></MemoryRouter></ThemeProvider>);

  await screen.findByRole("heading", { level: 2, name: "设置" });
  await expandAdvanced(user);
  const sync = await screen.findByRole("button", { name: "备份并同步" });
  await waitFor(() => expect(sync).toBeEnabled());
  expect(invokeMock).not.toHaveBeenCalledWith("start_provider_sync_preview_job", expect.anything());

  await user.click(sync);
  expect(await screen.findByText(/执行阶段会先扫描本机会话/)).toBeInTheDocument();
});

test("button operation failures open a focused error dialog", async () => {
  const user = userEvent.setup();
  invokeMock.mockImplementation((command: string) => command === "start_provider_sync_preview_job"
    ? Promise.reject(new Error("repository is busy"))
    : Promise.resolve(response(command)));
  render(<ThemeProvider><MemoryRouter initialEntries={["/settings"]}><App /></MemoryRouter></ThemeProvider>);

  await screen.findByRole("heading", { level: 2, name: "设置" });
  await expandAdvanced(user);
  const preview = await screen.findByRole("button", { name: "预览" });
  await waitFor(() => expect(preview).toBeEnabled());
  await user.click(preview);

  const dialog = await screen.findByRole("alertdialog", { name: "操作未完成" });
  expect(dialog).toHaveTextContent("repository is busy");
  expect(screen.getByRole("button", { name: "知道了" })).toHaveFocus();
  await user.click(screen.getByRole("button", { name: "知道了" }));
  expect(screen.queryByRole("alertdialog", { name: "操作未完成" })).not.toBeInTheDocument();
  expect(preview).toHaveFocus();
});
