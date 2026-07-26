export type ScanWarning = {
  kind: string;
  path: string;
  message: string;
};

export type ThreadBundle = {
  threadId: string;
  title: string;
  archived: boolean;
  modelProvider: string | null;
  workspace: {
    logicalId: string | null;
    sourcePath: string | null;
  };
};

export type ScanReport = {
  codexHome: string;
  databasePaths: string[];
  activeCount: number;
  archivedCount: number;
  totalRolloutBytes: number;
  totalCount: number;
  threads: ThreadBundle[];
  warnings: ScanWarning[];
};

export type SnapshotSummary = {
  snapshotId: string;
  manifestPath: string;
  threadCount: number;
  objectCount: number;
  totalBytes: number;
  warningCount: number;
};

export type SnapshotValidationReport = {
  snapshotId: string;
  manifestPath: string;
  threadCount: number;
  objectCount: number;
  totalBytes: number;
  valid: boolean;
};

export type ImportReport = {
  operationId: string;
  snapshotId: string;
  importedCount: number;
  skippedCount: number;
  backupDir: string;
  journalPath: string;
};

export type OperationJournal = {
  operationId: string;
  snapshotId: string;
  status: string;
  backupDir: string;
  error: string | null;
};

export type CodexProcess = {
  pid: number;
  name: string;
  executable: string | null;
  commandLine: string[];
  kind: "desktop" | "cli";
};

export type OperationProgress = {
  phase: string;
  message: string;
  completed: number;
  total: number | null;
  unit: string;
  cancellable: boolean;
};

export type JobState = "running" | "cancelling" | "completed" | "cancelled" | "failed";

export type JobSnapshot = {
  jobId: string;
  kind: "scan" | "snapshot" | "validate" | "import" | "recovery" | "push" | "pull" | "resolve" | "switch";
  state: JobState;
  progress: OperationProgress;
  cancellable: boolean;
  resultReady: boolean;
  error: string | null;
};

export type RemoteProfile = {
  id: string;
  displayName: string;
  serverUrl: string;
  selectedNamespaceId: string | null;
  createdAt: string;
  updatedAt: string;
};

export type RemoteProfileSummary = RemoteProfile & {
  credentialConfigured: boolean;
  insecureHttp: boolean;
};

export type RemoteNamespace = {
  id: string;
  displayName: string;
  head: string | null;
  createdAt: string;
  updatedAt: string;
};

export type ProtocolInfo = {
  service: string;
  version: string;
  protocolVersion: number;
};

export type RemoteConnectionStatus = {
  profile: RemoteProfileSummary;
  protocol: ProtocolInfo;
  namespaces: RemoteNamespace[];
};

export type RemoteNamespaceStatus = {
  remoteId: string;
  namespaceId: string;
  active: boolean;
  activeRemoteId: string | null;
  activeNamespaceId: string | null;
  integratedHead: string | null;
  remoteHead: string | null;
  generation: number | null;
};

export type ThreadConflictVersion = {
  title: string;
  archived: boolean;
  updatedAtMs: number | null;
  modelProvider: string | null;
  workspaceSourcePath: string | null;
  semanticHash: string;
};

export type ThreadConflict = {
  conflictId: string;
  threadId: string;
  title: string;
  kind: "both_modified" | "local_deleted_remote_modified" | "remote_deleted_local_modified";
  base: ThreadConflictVersion | null;
  local: ThreadConflictVersion | null;
  remote: ThreadConflictVersion | null;
};

export type ThreadConflictResolution = {
  conflictId: string;
  threadId: string;
  choice: "local" | "remote";
};

export type CheckoutReport = {
  operationId: string;
  snapshotId: string;
  threadCount: number;
  backupDir: string;
  localBackupDir: string;
  journalPath: string;
};

export type SyncReport = {
  kind: "pushed" | "pulled" | "merged" | "switched" | "no_changes" | "conflict";
  namespaceId: string;
  previousHead: string | null;
  head: string | null;
  revisionId: string | null;
  uploadedObjects: number;
  downloadedObjects: number;
  threadCount: number;
  conflicts: ThreadConflict[];
  checkout: CheckoutReport | null;
};
