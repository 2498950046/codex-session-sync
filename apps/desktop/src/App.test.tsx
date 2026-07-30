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

function response(command: string) {
  if (command === "get_default_codex_home") return "C:/Users/test/.codex";
  if (command === "get_default_repository_root") return "C:/Users/test/.codex-session-sync";
  if (command === "list_codex_processes") return runningProcesses ? [{ pid: 7, name: "Codex", executable: null, commandLine: [], kind: "desktop" }] : [];
  if (command === "list_remote_profiles") return [{ id: remoteId, displayName: "测试服务器", serverUrl: "https://sync.test", selectedNamespaceId: namespaceId, automaticNamespaceSelection: false, createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z", credentialConfigured: true, insecureHttp: false }];
  if (command === "list_remote_namespaces") return [{ id: namespaceId, displayName: "测试会话", head: "sha256:remote", createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z" }];
  if (command === "get_namespace_mapping_state") return { remoteId, automaticEnabled: false, context: { codexHomeKey: "c:/users/test/.codex", provider: "openai", apiKeyAvailable: false, apiKeyFingerprintHint: null, apiKeySource: null, warnings: [] }, mappings: [], selection: { selectedNamespaceId: namespaceId, source: "profile_default", matchedMappingId: null, ambiguousMappingIds: [] } };
  if (command === "get_remote_namespace_status") return { remoteId, namespaceId, active: true, activeRemoteId: remoteId, activeNamespaceId: namespaceId, integratedHead: "sha256:remote", remoteHead: "sha256:remote", generation: 2 };
  if (command === "get_workspace_mapping_state") return { remoteId, namespaceId, codexHomeKey: "c:/users/test/.codex", mappings: [] };
  throw new Error(`Unexpected command in test: ${command}`);
}

beforeEach(() => {
  runningProcesses = false;
  invokeMock.mockImplementation((command: string) => Promise.resolve(response(command)));
  Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
});

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
