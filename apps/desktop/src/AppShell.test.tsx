import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "./router";
import { expect, test } from "vitest";
import { AppShell } from "./AppShell";

test("sidebar navigation changes the active product page and remembers it", async () => {
  const user = userEvent.setup();
  render(<MemoryRouter initialEntries={["/sync"]}>
    <AppShell processes={[]} busy={false} onRefreshProcesses={() => undefined}>
      <Routes>
        <Route path="/sync" element={<div>同步内容</div>} />
        <Route path="/sessions" element={<div>会话内容</div>} />
      </Routes>
    </AppShell>
  </MemoryRouter>);

  expect(screen.getByRole("heading", { name: "同步" })).toBeInTheDocument();
  expect(screen.getByText("同步内容")).toBeInTheDocument();

  await user.click(screen.getByRole("link", { name: "会话" }));
  expect(screen.getByRole("heading", { name: "会话" })).toBeInTheDocument();
  expect(screen.getByText("会话内容")).toBeInTheDocument();
  expect(window.localStorage.getItem("codex-session-sync.last-route")).toBe("/sessions");
});

test("runtime status reports active Codex processes", () => {
  render(<MemoryRouter initialEntries={["/overview"]}>
    <AppShell
      busy={false}
      onRefreshProcesses={() => undefined}
      processes={[{ pid: 42, name: "Codex", executable: null, commandLine: [], kind: "desktop" }]}
    ><div /></AppShell>
  </MemoryRouter>);

  expect(screen.getByRole("button", { name: /Codex 运行中/ })).toBeInTheDocument();
});

test("top-level navigation is minimal: overview, sync, sessions, settings only", async () => {
  const user = userEvent.setup();
  render(<MemoryRouter initialEntries={["/overview"]}>
    <AppShell busy={false} onRefreshProcesses={() => undefined} processes={[]}>
      <Routes>
        <Route path="/overview" element={<div>概览内容</div>} />
        <Route path="/settings" element={<div>设置内容</div>} />
      </Routes>
    </AppShell>
  </MemoryRouter>);

  expect(screen.getByRole("link", { name: "概览" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "同步" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "会话" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "设置" })).toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "快照与恢复" })).not.toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "命名空间" })).not.toBeInTheDocument();
  expect(screen.queryByRole("link", { name: "高级工具" })).not.toBeInTheDocument();

  await user.click(screen.getByRole("link", { name: "设置" }));
  expect(screen.getByRole("heading", { name: "设置" })).toBeInTheDocument();
  expect(screen.getByText("设置内容")).toBeInTheDocument();
});
