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
