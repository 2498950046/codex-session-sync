import { mockIPC } from "@tauri-apps/api/mocks";
import type { JobSnapshot, NamespaceMappingState, ScanReport, SyncReport, WorkspaceMappingRule, WorkspacePullPlan } from "./types";

const remoteId = "019fa1a0-1111-7111-8111-111111111111";
const namespaceId = "019fa1a0-2222-7222-8222-222222222222";
const workNamespaceId = "019fa1a0-4444-7444-8444-444444444444";
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

const remapReport: SyncReport = {
  ...conflictReport,
  kind: "remapped",
  conflicts: [],
  checkout: {
    operationId: "019fa1a0-6666-7666-8666-666666666666",
    snapshotId: "019fa1a0-7777-7777-8777-777777777777",
    threadCount: 418,
    backupDir: "C:/Users/demo/.codex-session-sync/backups/remap",
    localBackupDir: "C:/Users/demo/.codex/.codex-session-sync/backups/remap",
    journalPath: "C:/Users/demo/.codex-session-sync/journal/checkout-remap.json",
  },
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

const namespaceMappingState: NamespaceMappingState = {
  remoteId,
  automaticEnabled: true,
  context: {
    codexHomeKey: "c:/users/demo/.codex",
    provider: "openai",
    apiKeyAvailable: true,
    apiKeyFingerprintHint: "5a91d4c2e731",
    apiKeySource: "auth_json",
    warnings: [],
  },
  mappings: [{
    id: "019fa1a0-3333-7333-8333-333333333333",
    remoteId,
    namespaceId: workNamespaceId,
    label: "工作账号",
    matchesApiKey: true,
    apiKeyFingerprintHint: "5a91d4c2e731",
    provider: "openai",
    codexHomeKey: null,
    createdAt: "2026-07-27T00:00:00Z",
    updatedAt: "2026-07-27T00:00:00Z",
  }],
  selection: {
    selectedNamespaceId: workNamespaceId,
    source: "mapping",
    matchedMappingId: "019fa1a0-3333-7333-8333-333333333333",
    ambiguousMappingIds: [],
  },
};

export async function installDevelopmentPreview(preview: "ready" | "empty" | "process-running" | "job" | "mapping" | "conflict" | "failure") {
  let automaticEnabled = true;
  let selectedProfileNamespaceId = namespaceId;
  let manualOverrideNamespaceId: string | null = null;
  let mappings = [...namespaceMappingState.mappings];
  let workspaceMappings: WorkspaceMappingRule[] = [
    {
      id: "019fa1a0-5555-7555-8555-555555555555",
      remoteId,
      namespaceId: workNamespaceId,
      codexHomeKey: "c:/users/demo/.codex",
      remotePrefix: "D:/projects/cpa",
      localPrefix: "F:/history/cpa",
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
    },
    {
      id: "019fa1a0-6666-7666-8666-666666666666",
      remoteId,
      namespaceId: workNamespaceId,
      codexHomeKey: "c:/users/demo/.codex",
      remotePrefix: "D:/yaxin",
      localPrefix: "F:/history/yaxin",
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
    },
  ];
  let workspaceCleanupPaths = [
    "F:/history/cpa-3",
    "F:/history/do-c-2",
    "F:/history/new-chat-5",
  ];

  function workspacePullPlan(requestedNamespaceId: string): WorkspacePullPlan {
    return {
      remoteId,
      namespaceId: requestedNamespaceId,
      remoteHead: requestedNamespaceId === workNamespaceId ? remoteHead : baseHash,
      mappedPathCount: workspaceMappings.length,
      existingPathCount: 1,
      unmappedPaths: workspaceMappings.length > 0 ? [] : [{
        remotePath: "D:/projects/codex-session-sync",
        suggestedSubdirectory: "codex-session-sync",
      }, {
        remotePath: "/Users/demo/work/notes",
        suggestedSubdirectory: "notes",
      }],
    };
  }

  function currentMappingState(): NamespaceMappingState {
    const mappedNamespaceId = mappings[0]?.namespaceId ?? null;
    return {
      ...namespaceMappingState,
      automaticEnabled,
      mappings,
      selection: automaticEnabled
        ? manualOverrideNamespaceId
          ? {
              selectedNamespaceId: manualOverrideNamespaceId,
              source: "manual_override",
              matchedMappingId: null,
              ambiguousMappingIds: [],
            }
          : mappedNamespaceId
            ? {
                selectedNamespaceId: mappedNamespaceId,
                source: "mapping",
                matchedMappingId: mappings[0].id,
                ambiguousMappingIds: [],
              }
            : {
                selectedNamespaceId: selectedProfileNamespaceId,
                source: "profile_default",
                matchedMappingId: null,
                ambiguousMappingIds: [],
              }
        : {
            selectedNamespaceId: selectedProfileNamespaceId,
            source: "profile_default",
            matchedMappingId: null,
            ambiguousMappingIds: [],
          },
    };
  }

  mockIPC((command, args) => {
    if (command === "get_default_codex_home") return "C:/Users/demo/.codex";
    if (command === "get_default_repository_root") return "C:/Users/demo/.codex-session-sync";
    if (command === "list_codex_processes") return preview === "process-running" ? [{
      pid: 4242,
      name: "Codex",
      executable: "C:/Program Files/Codex/Codex.exe",
      commandLine: [],
      kind: "desktop",
    }] : [];
    if (command === "list_remote_profiles") return preview === "empty" ? [] : [{
      id: remoteId,
      displayName: "个人服务器",
      serverUrl: "https://sync.example.test",
      selectedNamespaceId: selectedProfileNamespaceId,
      automaticNamespaceSelection: automaticEnabled,
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
      credentialConfigured: true,
      insecureHttp: false,
    }];
    if (command === "list_remote_namespaces") return [{
      id: namespaceId,
      displayName: "个人会话",
      head: baseHash,
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
    }, {
      id: workNamespaceId,
      displayName: "工作会话",
      head: remoteHead,
      createdAt: "2026-07-27T00:00:00Z",
      updatedAt: "2026-07-27T00:00:00Z",
    }];
    if (command === "get_remote_namespace_status") {
      const requestedNamespaceId = String((args as { namespaceId?: string } | undefined)?.namespaceId);
      return {
      remoteId,
      namespaceId: requestedNamespaceId,
      active: requestedNamespaceId === namespaceId,
      activeRemoteId: remoteId,
      activeNamespaceId: namespaceId,
      integratedHead: baseHash,
      remoteHead: requestedNamespaceId === workNamespaceId ? remoteHead : baseHash,
      generation: 2,
      };
    }
    if (command === "get_namespace_mapping_state") return currentMappingState();
    if (command === "get_workspace_mapping_state") return {
      remoteId,
      namespaceId: String((args as { namespaceId?: string } | undefined)?.namespaceId ?? namespaceId),
      codexHomeKey: "c:/users/demo/.codex",
      mappings: workspaceMappings,
    };
    if (command === "get_workspace_cleanup_report") return {
      scannedRoots: ["F:/history"],
      entries: [
        { path: "F:/history/cpa", activeCount: 1, archivedCount: 2, mappings: workspaceMappings.filter((mapping) => mapping.localPrefix === "F:/history/cpa").map((mapping) => ({ id: mapping.id, remotePrefix: mapping.remotePrefix, localPrefix: mapping.localPrefix, inherited: false })), codexProjectNames: ["cpa"], directoryState: "nonEmpty", cleanupEligible: false },
        { path: "F:/history/do-c", activeCount: 3, archivedCount: 1, mappings: [], codexProjectNames: ["do-c"], directoryState: "nonEmpty", cleanupEligible: false },
        { path: "F:/history/yaxin", activeCount: 0, archivedCount: 4, mappings: workspaceMappings.filter((mapping) => mapping.localPrefix === "F:/history/yaxin").map((mapping) => ({ id: mapping.id, remotePrefix: mapping.remotePrefix, localPrefix: mapping.localPrefix, inherited: false })), codexProjectNames: ["yaxin"], directoryState: "nonEmpty", cleanupEligible: false },
        { path: "F:/history/yaxin/data-platform", activeCount: 5, archivedCount: 0, mappings: workspaceMappings.filter((mapping) => mapping.localPrefix === "F:/history/yaxin").map((mapping) => ({ id: mapping.id, remotePrefix: mapping.remotePrefix, localPrefix: mapping.localPrefix, inherited: true })), codexProjectNames: ["data-platform"], directoryState: "missing", cleanupEligible: false },
        ...workspaceCleanupPaths.map((path) => ({ path, activeCount: 0, archivedCount: 0, mappings: [], codexProjectNames: path.endsWith("new-chat-5") ? [] : [path.split("/").at(-1) ?? path], directoryState: path.endsWith("do-c-2") ? "missing" : "empty", cleanupEligible: true })),
      ],
      candidates: workspaceCleanupPaths.map((path) => ({ path })),
    };
    if (command === "quarantine_workspace_directories") {
      const paths = (args as { request?: { paths?: string[] } } | undefined)?.request?.paths ?? [];
      workspaceCleanupPaths = workspaceCleanupPaths.filter((path) => !paths.includes(path));
      return {
        quarantined: paths.filter((path) => !path.endsWith("do-c-2")).map((path) => ({
          originalPath: path,
          quarantinePath: `C:/Users/demo/.codex-session-sync/quarantine/empty-workspaces/${path.split("/").at(-1)}`,
        })),
        removedCodexProjects: paths.filter((path) => !path.endsWith("new-chat-5")).length,
        removedThreadAssignments: 2,
        backupPath: "C:/Users/demo/.codex-session-sync/backups/workspace-cleanup-preview/codex-global-state.json",
        journalPath: "C:/Users/demo/.codex-session-sync/journal/workspace-cleanup-preview.json",
      };
    }
    if (command === "get_workspace_pull_plan") {
      const requestedNamespaceId = String((args as { namespaceId?: string } | undefined)?.namespaceId ?? namespaceId);
      return workspacePullPlan(requestedNamespaceId);
    }
    if (command === "create_automatic_workspace_mappings") {
      const request = (args as { request?: { namespaceId?: string; mappings?: Array<{ remotePath: string; localPath: string }> } } | undefined)?.request;
      const plan = workspacePullPlan(String(request?.namespaceId ?? namespaceId));
      workspaceMappings = (request?.mappings ?? []).map((mapping, index) => ({
        id: `019fa1a0-5555-7555-8555-55555555555${index}`,
        remoteId,
        namespaceId: plan.namespaceId,
        codexHomeKey: "c:/users/demo/.codex",
        remotePrefix: mapping.remotePath,
        localPrefix: mapping.localPath,
        createdAt: "2026-07-28T00:00:00Z",
        updatedAt: "2026-07-28T00:00:00Z",
      }));
      return {
        state: { remoteId, namespaceId: plan.namespaceId, codexHomeKey: "c:/users/demo/.codex", mappings: workspaceMappings },
        createdDirectories: workspaceMappings.map((mapping) => mapping.localPrefix),
      };
    }
    if (command === "create_workspace_mapping") {
      const request = (args as { request?: { namespaceId?: string; remotePrefix?: string; localPrefix?: string } } | undefined)?.request;
      workspaceMappings = [...workspaceMappings, {
        id: "019fa1a0-5555-7555-8555-555555555555",
        remoteId,
        namespaceId: String(request?.namespaceId ?? namespaceId),
        codexHomeKey: "c:/users/demo/.codex",
        remotePrefix: String(request?.remotePrefix ?? "D:/projects"),
        localPrefix: String(request?.localPrefix ?? "F:/workspace"),
        createdAt: "2026-07-28T00:00:00Z",
        updatedAt: "2026-07-28T00:00:00Z",
      }];
      return { remoteId, namespaceId: request?.namespaceId ?? namespaceId, codexHomeKey: "c:/users/demo/.codex", mappings: workspaceMappings };
    }
    if (command === "delete_workspace_mapping") {
      const mappingId = String((args as { mappingId?: string } | undefined)?.mappingId);
      workspaceMappings = workspaceMappings.filter((mapping) => mapping.id !== mappingId);
      return { remoteId, namespaceId, codexHomeKey: "c:/users/demo/.codex", mappings: workspaceMappings };
    }
    if (command === "set_automatic_namespace_selection") {
      automaticEnabled = Boolean((args as { enabled?: boolean } | undefined)?.enabled);
      if (automaticEnabled) manualOverrideNamespaceId = null;
      return currentMappingState();
    }
    if (command === "clear_manual_namespace_override") {
      manualOverrideNamespaceId = null;
      return currentMappingState();
    }
    if (command === "create_namespace_mapping") return currentMappingState();
    if (command === "delete_namespace_mapping") {
      const mappingId = String((args as { mappingId?: string } | undefined)?.mappingId);
      mappings = mappings.filter((mapping) => mapping.id !== mappingId);
      return currentMappingState();
    }
    if (command === "start_pull_job") return job("preview-pull", "pull", "running");
    if (command === "start_workspace_remap_job") return job("preview-remap", "remap", "running");
    if (command === "start_scan_job") return job("preview-scan", "scan", "running");
    if (command === "get_job") {
      const jobId = String((args as { jobId?: string } | undefined)?.jobId);
      if (preview === "job") return job(jobId, jobId === "preview-scan" ? "scan" : "pull", "running");
      if (preview === "failure") return {
        ...job(jobId, jobId === "preview-scan" ? "scan" : "pull", "failed"),
        error: "预览任务失败：服务器返回的对象未通过哈希校验。",
      };
      return job(jobId, jobId === "preview-scan" ? "scan" : jobId === "preview-remap" ? "remap" : "pull", "completed");
    }
    if (command === "take_job_result") {
      const jobId = (args as { jobId?: string } | undefined)?.jobId;
      return jobId === "preview-scan" ? scanReport : jobId === "preview-remap" ? remapReport : conflictReport;
    }
    if (command === "select_remote_namespace") {
      const selected = String((args as { namespaceId?: string } | undefined)?.namespaceId);
      selectedProfileNamespaceId = selected;
      if (automaticEnabled) manualOverrideNamespaceId = selected;
      return null;
    }
    throw new Error(`Conflict preview does not implement command: ${command}`);
  });
}
