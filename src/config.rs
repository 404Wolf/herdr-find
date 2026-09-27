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
    /// The Herdr pane this process runs in (the calling agent's pane).
    pub current_pane: Option<String>,
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
    /// | `HERDR_PANE_ID` | the caller's own pane, excluded from search results |
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
            current_pane: env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty()),
        }
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
}
