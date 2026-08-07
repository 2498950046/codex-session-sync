export type ScanWarning = {
  kind: string;
  path: string;
  message: string;
};

export type QuarantinedRollout = {
  originalPath: string;
  quarantinePath: string;
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

export type ProviderSyncPreview = {
  provider: string;
  rolloutCount: number;
  rolloutBytes: number;
  databaseRowCount: number;
  catalogDatabaseCount: number;
  warnings: ScanWarning[];
  noChanges: boolean;
};

export type ProviderSyncReport = {
  operationId: string;
  provider: string;
  rolloutCount: number;
  databaseRowCount: number;
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
  kind: "scan" | "snapshot" | "validate" | "import" | "provider_sync_preview" | "provider_sync" | "recovery" | "restore" | "revision-download" | "revision-restore" | "revision-publish" | "push" | "pull" | "resolve" | "switch" | "remap";
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
  automaticNamespaceSelection: boolean;
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
  namespaceEpoch: number;
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

export type ApiKeySource = "transient_input" | "provider_environment" | "auth_json";

export type LocalIdentitySummary = {
  codexHomeKey: string;
  provider: string | null;
  apiKeyAvailable: boolean;
  apiKeyFingerprintHint: string | null;
  apiKeySource: ApiKeySource | null;
  warnings: string[];
};

export type NamespaceMappingSummary = {
  id: string;
  remoteId: string;
  namespaceId: string;
  label: string;
  matchesApiKey: boolean;
  apiKeyFingerprintHint: string | null;
  provider: string | null;
  codexHomeKey: string | null;
  createdAt: string;
  updatedAt: string;
};

export type NamespaceSelectionSource = "manual_override" | "mapping" | "profile_default" | "ambiguous" | "none";

export type NamespaceMappingState = {
  remoteId: string;
  automaticEnabled: boolean;
  context: LocalIdentitySummary;
  mappings: NamespaceMappingSummary[];
  selection: {
    selectedNamespaceId: string | null;
    source: NamespaceSelectionSource;
    matchedMappingId: string | null;
    ambiguousMappingIds: string[];
  };
};

export type SnapshotMetadata = {
  description: string;
  tags: string[];
  pinned: boolean;
  automatic: boolean;
};

export type LocalSnapshotListItem = {
  snapshotId: string;
  createdAt: string;
  manifestPath: string;
  threadCount: number;
  objectCount: number;
  logicalBytes: number;
  physicalReferencedBytes: number;
  warningCount: number;
  metadata: SnapshotMetadata;
};

export type RevisionSummary = {
  revisionId: string;
  namespaceId: string;
  parentRevision: string | null;
  createdAt: string;
  threadCount: number;
  objectCount: number;
  logicalBytes: number;
  physicalReferencedBytes: number;
  state: "active" | "trashed";
};

export type SnapshotDeletionPlan = {
  snapshotId: string;
  manifestPath: string;
  pinned: boolean;
  protectedByOperations?: string[];
  sharedObjectCount: number;
  exclusiveObjectCount: number;
  estimatedReclaimableBytes: number;
  planFingerprint: string;
};

export type SnapshotTrashEntry = {
  operationId: string;
  snapshotId: string;
  trashedAt: string;
  originalManifestPath: string;
  trashManifestPath: string;
};

export type RemoteHistoryTrashOperation = {
  operationId: string;
  namespaceId: string;
  oldHead: string | null;
  newHead: string | null;
  epochBefore: number;
  epochAfter: number;
  createdAt: string;
  expiresAt: string;
  revisionCount: number;
  state: string;
};

export type GcPlan = {
  schemaVersion: number;
  createdAt: string;
  reachableObjects: number;
  unreachableObjects: Array<{ kind: string; sha256: string; byteLength: number }>;
  reclaimableBytes: number;
};

export type RepositoryStorageSummary = {
  logicalBytes: number;
  repositoryPhysicalBytes: number;
  activePhysicalBytes: number;
  sharedPhysicalBytes: number;
  exclusivePhysicalBytes: number;
  trashBytes: number;
  gcQuarantineBytes: number;
  reclaimableBytes: number;
  protectedByJournalBytes: number;
};

export type RecoveryPoint = {
  operationId: string;
  kind: "import" | "checkout" | "provider_sync";
  status: string;
  journalPath: string;
  targetCodexHome: string;
  startedAt: string | null;
  updatedAt: string | null;
  requiresAttention: boolean;
};

export type WorkspaceMappingRule = {
  id: string;
  remoteId: string;
  namespaceId: string;
  codexHomeKey: string;
  remotePrefix: string;
  localPrefix: string;
  createdAt: string;
  updatedAt: string;
};

export type WorkspaceMappingState = {
  remoteId: string;
  namespaceId: string;
  codexHomeKey: string;
  mappings: WorkspaceMappingRule[];
};

export type WorkspaceDirectoryState = "unknown" | "missing" | "empty" | "nonEmpty" | "notDirectory";

export type WorkspacePathFilter = "all" | "active" | "archived" | "codexProject" | "mapped" | "cleanup";

export type WorkspacePathEntry = {
  path: string;
  activeCount: number;
  archivedCount: number;
  mappings: Array<{
    id: string;
    remotePrefix: string;
    localPrefix: string;
    inherited: boolean;
  }>;
  codexProjectNames: string[];
  directoryState: WorkspaceDirectoryState;
  cleanupEligible: boolean;
};

export type WorkspaceCleanupCandidate = { path: string };

export type WorkspaceCleanupReport = {
  scannedRoots: string[];
  entries: WorkspacePathEntry[];
  candidates: WorkspaceCleanupCandidate[];
};

export type WorkspaceCleanupResult = {
  quarantined: Array<{
    originalPath: string;
    quarantinePath: string;
  }>;
  removedCodexProjects: number;
  removedThreadAssignments: number;
  backupPath: string | null;
  journalPath: string;
};

export type WorkspacePathCandidate = {
  remotePath: string;
  suggestedSubdirectory: string;
};

export type WorkspacePullPlan = {
  remoteId: string;
  namespaceId: string;
  remoteHead: string | null;
  mappedPathCount: number;
  existingPathCount: number;
  unmappedPaths: WorkspacePathCandidate[];
};

export type AutomaticWorkspaceMappingResult = {
  state: WorkspaceMappingState;
  createdDirectories: string[];
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
  kind: "pushed" | "pulled" | "merged" | "switched" | "remapped" | "no_changes" | "conflict";
  namespaceId: string;
  previousHead: string | null;
  head: string | null;
  revisionId: string | null;
  uploadedObjects: number;
  downloadedObjects: number;
  threadCount: number;
  conflicts: ThreadConflict[];
  checkout: CheckoutReport | null;
  pushMetrics?: {
    missingQueryMs: number;
    uploadMs: number;
    commitMs: number;
    transferredObjects: number;
    transferredBytes: number;
    createdObjects: number;
    maxConcurrency: number;
  };
};
