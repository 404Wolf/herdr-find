//! Runtime configuration, read from the environment.

use std::env;
use std::path::{Path, PathBuf};

/// Where to find things on this machine.
#[derive(Debug, Clone)]
pub struct Config {
    /// Claude Code's config dir (`~/.claude`).
    pub claude_dir: PathBuf,
    /// Codex's home dir (`~/.codex`).
    pub codex_dir: PathBuf,
    /// A procfs root to read instead of the system one (tests).
    pub proc_dir: Option<PathBuf>,
    /// The herdr binary to run.
    pub herdr_bin: String,
    /// The herdr-core CLI to run.
    pub hc_bin: String,
    /// The Herdr app CLI to run.
    pub app_bin: String,
    /// Runtimes to search: the Herdr TUI (`herdr`) and/or herdr-core (`core`).
    pub runtimes: Vec<String>,
    /// The panes / terminals this process runs in (the calling agent's).
    pub current_panes: Vec<String>,
}

impl Config {
    /// Build the configuration from environment variables.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `HERDR_FIND_CLAUDE_DIR`, then `CLAUDE_CONFIG_DIR` | Claude Code dir (default `~/.claude`) |
    /// | `HERDR_FIND_CODEX_DIR`, then `CODEX_HOME` | Codex dir (default `~/.codex`) |
    /// | `HERDR_FIND_PROC_DIR` | procfs root (default `/proc`) |
    /// | `HERDR_BIN_PATH` | herdr binary (default `herdr` on `PATH`) |
    /// | `HC_BIN_PATH` | herdr-core CLI (default `hc` on `PATH`) |
    /// | `HERDR_APP_BIN_PATH` | Herdr app CLI (default `herdr-app` on `PATH`) |
    /// | `HERDR_FIND_RUNTIMES` | comma-separated: `herdr` (TUI), `core` (default both) |
    /// | `HERDR_PANE_ID`, `HC_TERMINAL_ID` | the caller's own pane / terminal, excluded from search results |
    pub fn from_env() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dir = |vars: &[&str], default: &str| {
            vars.iter()
                .find_map(|v| env::var_os(v).filter(|s| !s.is_empty()))
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        Self {
            claude_dir: dir(&["HERDR_FIND_CLAUDE_DIR", "CLAUDE_CONFIG_DIR"], ".claude"),
            codex_dir: dir(&["HERDR_FIND_CODEX_DIR", "CODEX_HOME"], ".codex"),
            proc_dir: env::var_os("HERDR_FIND_PROC_DIR")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            herdr_bin: herdr_bin(env::var("HERDR_BIN_PATH").ok().as_deref()),
            hc_bin: bin("HC_BIN_PATH", "hc"),
            app_bin: bin("HERDR_APP_BIN_PATH", "herdr-app"),
            runtimes: runtimes(env::var("HERDR_FIND_RUNTIMES").ok().as_deref()),
            current_panes: ["HERDR_PANE_ID", "HC_TERMINAL_ID"]
                .iter()
                .filter_map(|v| env::var(v).ok().filter(|s| !s.is_empty()))
                .collect(),
        }
    }
}

fn bin(var: &str, default: &str) -> String {
    env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.into())
}

/// Parse `HERDR_FIND_RUNTIMES` (default: both).
pub fn runtimes(var: Option<&str>) -> Vec<String> {
    let picked: Vec<String> = var
        .unwrap_or_default()
        .split(',')
        .map(|s| match s.trim() {
            "tui" => "herdr".to_string(),
            "hc" | "herdr-core" => "core".to_string(),
            other => other.to_string(),
        })
        .filter(|s| s == "herdr" || s == "core")
        .collect();
    if picked.is_empty() {
        vec!["herdr".into(), "core".into()]
    } else {
        picked
    }
}

/// Resolve the herdr binary from `HERDR_BIN_PATH`.
///
/// Herdr exports its own `/proc/self/exe`, which reads `<path> (deleted)` once
/// the binary has been replaced on disk (e.g. after an upgrade). Strip that
/// suffix, and fall back to `herdr` on `PATH` when the file is gone.
pub fn herdr_bin(var: Option<&str>) -> String {
    match var.map(str::trim).filter(|s| !s.is_empty()) {
        None => "herdr".into(),
        Some(raw) => match raw.strip_suffix(" (deleted)") {
            Some(path) if !Path::new(path).exists() => "herdr".into(),
            Some(path) => path.into(),
            None => raw.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_bin_defaults_and_strips_deleted() {
        assert_eq!(herdr_bin(None), "herdr");
        assert_eq!(herdr_bin(Some("")), "herdr");
        assert_eq!(herdr_bin(Some("herdr-dev")), "herdr-dev");
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        assert_eq!(herdr_bin(Some(&format!("{exe} (deleted)"))), exe);
        assert_eq!(herdr_bin(Some("/nonexistent/bin/herdr (deleted)")), "herdr");
        assert_eq!(
            herdr_bin(Some("/nonexistent/bin/herdr")),
            "/nonexistent/bin/herdr"
        );
    }

    #[test]
    fn runtimes_default_to_both() {
        assert_eq!(runtimes(None), ["herdr", "core"]);
        assert_eq!(runtimes(Some("")), ["herdr", "core"]);
        assert_eq!(runtimes(Some("hc")), ["core"]);
        assert_eq!(runtimes(Some("herdr, core")), ["herdr", "core"]);
        assert_eq!(runtimes(Some("bogus")), ["herdr", "core"]);
    }
}
