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
