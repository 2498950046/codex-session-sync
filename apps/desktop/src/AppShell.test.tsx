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
