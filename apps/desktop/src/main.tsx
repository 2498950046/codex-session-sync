import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { HashRouter } from "./router";
import App from "./App";
import { ThemeProvider } from "./theme";
import "./styles.css";

async function bootstrap() {
  const preview = new URLSearchParams(window.location.search).get("preview");
  if (!window.location.hash) {
    const lastRoute = window.localStorage.getItem("codex-session-sync.last-route");
    const previewRoute = preview === "history" ? "/sync"
      : preview === "mapping" ? "/settings"
      : preview === "conflict" || preview === "job" || preview === "failure" ? "/sync"
        : preview === "empty" || preview === "process-running" || preview === "ready" ? "/overview"
          : null;
    window.location.hash = previewRoute ?? (lastRoute?.startsWith("/") ? lastRoute : "/overview");
  }
  if (import.meta.env.DEV && ["ready", "empty", "process-running", "job", "mapping", "conflict", "failure", "history"].includes(preview ?? "")) {
    const { installDevelopmentPreview } = await import("./dev-conflict-preview");
    await installDevelopmentPreview(preview as "ready" | "empty" | "process-running" | "job" | "mapping" | "conflict" | "failure" | "history");
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <ThemeProvider>
        <HashRouter>
          <App />
        </HashRouter>
      </ThemeProvider>
    </StrictMode>,
  );
}

void bootstrap();
