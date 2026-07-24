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
  kind: "scan" | "snapshot" | "validate" | "import" | "recovery";
  state: JobState;
  progress: OperationProgress;
  cancellable: boolean;
  resultReady: boolean;
  error: string | null;
};
