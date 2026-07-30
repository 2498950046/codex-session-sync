import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { ThemeProvider, useTheme } from "./theme";

function ThemeProbe() {
  const theme = useTheme();
  return <div>
    <span>{theme.preference}:{theme.resolvedTheme}</span>
    <button onClick={() => theme.setPreference("dark")}>深色</button>
  </div>;
}

test("theme defaults to the system and persists a manual choice", async () => {
  const user = userEvent.setup();
  render(<ThemeProvider><ThemeProbe /></ThemeProvider>);

  expect(screen.getByText("system:light")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "深色" }));

  expect(screen.getByText("dark:dark")).toBeInTheDocument();
  expect(document.documentElement).toHaveAttribute("data-theme", "dark");
  expect(window.localStorage.getItem("codex-session-sync.theme")).toBe("dark");
});
