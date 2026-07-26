import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles.css";

async function bootstrap() {
  const preview = new URLSearchParams(window.location.search).get("preview");
  if (import.meta.env.DEV && (preview === "conflict" || preview === "mapping")) {
    const { installDevelopmentPreview } = await import("./dev-conflict-preview");
    await installDevelopmentPreview(preview);
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void bootstrap();
