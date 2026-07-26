import { mockIPC } from "@tauri-apps/api/mocks";
import type { JobSnapshot, ScanReport, SyncReport } from "./types";

const remoteId = "019fa1a0-1111-7111-8111-111111111111";
const namespaceId = "019fa1a0-2222-7222-8222-222222222222";
const remoteHead = `sha256:${"c".repeat(64)}`;
const baseHash = `sha256:${"a".repeat(64)}`;
const localHash = `sha256:${"b".repeat(64)}`;

function job(jobId: string, kind: JobSnapshot["kind"], state: JobSnapshot["state"]): JobSnapshot {
  return {
    jobId,
    kind,
    state,
    progress: {
      phase: state === "completed" ? "completed" : "pull_objects",
      message: state === "completed" ? "任务已完成" : "正在比较本地和远端会话",
      completed: state === "completed" ? 1 : 0,
      total: 1,
      unit: "tasks",
      cancellable: state !== "completed",
    },
    cancellable: state !== "completed",
    resultReady: state === "completed",
    error: null,
  };
}

const conflictReport: SyncReport = {
  kind: "conflict",
  namespaceId,
  previousHead: baseHash,
  head: remoteHead,
  revisionId: remoteHead,
  uploadedObjects: 0,
  downloadedObjects: 1,
  threadCount: 417,
  checkout: null,
  conflicts: [{
    conflictId: `sha256:${"d".repeat(64)}`,
    threadId: "019f9f23-fb50-71b3-a104-15bac2b8e9a5",
    title: "Codex 会话同步设计",
    kind: "both_modified",
    base: {
      title: "初始同步方案",
      archived: false,
      updatedAtMs: 1785033000000,
      modelProvider: "openai",
      workspaceSourcePath: "D:/codex-session-sync",
      semanticHash: baseHash,
    },
    local: {
      title: "Codex 会话同步设计",
      archived: false,
      updatedAtMs: 1785114900000,
      modelProvider: "openai",
      workspaceSourcePath: "D:/codex-session-sync",
      semanticHash: localHash,
    },
    remote: {
      title: "Codex 同步冲突处理",
      archived: false,
      updatedAtMs: 1785115200000,
      modelProvider: "openai",
      workspaceSourcePath: "/Users/me/codex-session-sync",
      semanticHash: remoteHead,
    },
  }],
};

const scanReport: ScanReport = {
  codexHome: "C:/Users/demo/.codex",
  databasePaths: ["C:/Users/demo/.codex/state_5.sqlite"],
  activeCount: 417,
  archivedCount: 1,
  totalRolloutBytes: 984_811_738,
  totalCount: 418,
  threads: [],
  warnings: [],
};

export async function installConflictPreview() {
  mockIPC((command, args) => {
    if (command === "get_default_codex_home") return "C:/Users/demo/.codex";
    if (command === "get_default_repository_root") return "C:/Users/demo/.codex-session-sync";
    if (command === "list_codex_processes") return [];
    if (command === "list_remote_profiles") return [{
      id: remoteId,
      displayName: "个人服务器",
      serverUrl: "https://sync.example.test",
      selectedNamespaceId: namespaceId,
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
      credentialConfigured: true,
      insecureHttp: false,
    }];
    if (command === "list_remote_namespaces") return [{
      id: namespaceId,
      displayName: "个人会话",
      head: remoteHead,
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
    }];
    if (command === "get_remote_namespace_status") return {
      remoteId,
      namespaceId,
      active: true,
      activeRemoteId: remoteId,
      activeNamespaceId: namespaceId,
      integratedHead: baseHash,
      remoteHead,
      generation: 2,
    };
    if (command === "start_pull_job") return job("preview-pull", "pull", "running");
    if (command === "start_scan_job") return job("preview-scan", "scan", "running");
    if (command === "get_job") {
      const jobId = String((args as { jobId?: string } | undefined)?.jobId);
      return job(jobId, jobId === "preview-scan" ? "scan" : "pull", "completed");
    }
    if (command === "take_job_result") {
      return (args as { jobId?: string } | undefined)?.jobId === "preview-scan"
        ? scanReport
        : conflictReport;
    }
    if (command === "select_remote_namespace") return null;
    throw new Error(`Conflict preview does not implement command: ${command}`);
  });
}
