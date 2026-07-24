use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sysinfo::{ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexProcessKind {
    Desktop,
    Cli,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProcess {
    pub pid: u32,
    pub name: String,
    pub executable: Option<PathBuf>,
    pub command_line: Vec<String>,
    pub kind: CodexProcessKind,
}

pub fn detect_codex_processes() -> Vec<CodexProcess> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut processes = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            classify_process(
                pid.as_u32(),
                &process.name().to_string_lossy(),
                process.exe().map(PathBuf::from),
                process
                    .cmd()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    processes.sort_by_key(|process| process.pid);
    processes
}

fn classify_process(
    pid: u32,
    name: &str,
    executable: Option<PathBuf>,
    command_line: Vec<String>,
) -> Option<CodexProcess> {
    let executable_text = executable
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let combined = format!("{name}\n{executable_text}\n{}", command_line.join("\n"));
    let normalized = combined.to_ascii_lowercase().replace('\\', "/");
    if normalized.contains("codex-session-sync") {
        return None;
    }

    let kind = if normalized.contains("@openai/codex")
        || normalized.contains("/node_modules/codex/")
        || normalized.contains("/node_modules/@openai/codex/")
        || normalized.contains("codex-cli")
    {
        Some(CodexProcessKind::Cli)
    } else if is_codex_desktop_name(name)
        || normalized.contains("/codex.app/")
        || normalized.contains("codex desktop")
    {
        Some(CodexProcessKind::Desktop)
    } else if is_codex_cli_name(name) {
        Some(CodexProcessKind::Cli)
    } else {
        None
    }?;

    Some(CodexProcess {
        pid,
        name: name.to_string(),
        executable,
        command_line,
        kind,
    })
}

fn is_codex_desktop_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "codex" | "codex.exe")
}

fn is_codex_cli_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "codex-cli" | "codex-cli.exe"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_windows_codex_desktop() {
        let process = classify_process(
            42,
            "Codex.exe",
            Some(PathBuf::from(
                r"C:\\Users\\user\\AppData\\Local\\Codex\\Codex.exe",
            )),
            vec![],
        )
        .unwrap();
        assert_eq!(process.kind, CodexProcessKind::Desktop);
    }

    #[test]
    fn identifies_macos_desktop_from_executable_path() {
        let process = classify_process(
            42,
            "Codex Helper",
            Some(PathBuf::from(
                "/Applications/Codex.app/Contents/MacOS/Codex",
            )),
            vec![],
        )
        .unwrap();
        assert_eq!(process.kind, CodexProcessKind::Desktop);
    }

    #[test]
    fn identifies_cli_from_openai_package_command() {
        let process = classify_process(
            42,
            "node",
            Some(PathBuf::from("/usr/bin/node")),
            vec!["/usr/lib/node_modules/@openai/codex/bin/codex.js".to_string()],
        )
        .unwrap();
        assert_eq!(process.kind, CodexProcessKind::Cli);
    }

    #[test]
    fn excludes_this_application() {
        assert!(
            classify_process(
                42,
                "codex-session-sync.exe",
                Some(PathBuf::from(r"C:\\apps\\codex-session-sync.exe")),
                vec![],
            )
            .is_none()
        );
    }
}
