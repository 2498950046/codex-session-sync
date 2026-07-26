import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles.css";

async function bootstrap() {
  if (import.meta.env.DEV && new URLSearchParams(window.location.search).get("preview") === "conflict") {
    const { installConflictPreview } = await import("./dev-conflict-preview");
    await installConflictPreview();
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void bootstrap();
