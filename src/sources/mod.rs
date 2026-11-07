//! Agent sources: one module per coding agent, each knowing how to find the
//! transcript behind a running agent process and how to read it.
//!
//! Adding an agent is one module implementing [`AgentSource`] plus one line in
//! [`default_sources`].

use std::path::PathBuf;

use crate::config::Config;
use crate::procs::ProcessTable;
use crate::transcript::Transcript;

pub mod claude;
pub mod codex;

/// A transcript that belongs to a live agent process in a Herdr pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The agent process.
    pub pid: u32,
    /// The Herdr pane it runs in.
    pub pane_id: String,
    /// Its transcript file.
    pub path: PathBuf,
}

/// One kind of coding agent (Claude Code, Codex, ...).
pub trait AgentSource: Send + Sync {
    /// The agent name Herdr reports for panes running this agent
    /// (`herdr agent list` → `agent`).
    fn kind(&self) -> &str;

    /// Transcripts of the live processes of this agent. Several candidates
    /// may name the same pane (e.g. an agent started from inside another);
    /// the caller picks the outermost process.
    fn locate(&self, procs: &dyn ProcessTable) -> Vec<Candidate>;

    /// Whether a command line is this agent's own process.
    fn is_agent(&self, argv: &[String]) -> bool;

    /// The transcript one live process of this agent is writing, if any.
    fn transcript_of_process(&self, pid: u32, procs: &dyn ProcessTable) -> Option<PathBuf>;

    /// The transcript of a session id (e.g. from an `agent.session` label).
    fn transcript_of_session(&self, session_id: &str) -> Option<PathBuf>;

    /// Fold one JSONL line of a transcript into `out`. Lines may be cut short
    /// or not JSON at all; ignore anything unrecognised.
    fn parse_line(&self, line: &str, out: &mut Transcript);
}

/// The agent sources enabled by default.
pub fn default_sources(config: &Config) -> Vec<Box<dyn AgentSource>> {
    vec![
        Box::new(claude::Claude::new(&config.claude_dir)),
        Box::new(codex::Codex::new(&config.codex_dir)),
    ]
}

/// The base name of a program path (`/nix/store/.../bin/claude` → `claude`).
pub(crate) fn program_name(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

/// Whether `argv` runs `name`: directly, or as a script under node/bun/deno
/// whose path mentions `package`.
pub(crate) fn runs(argv: &[String], name: &str, package: &str) -> bool {
    let Some(program) = argv.first().map(|a| program_name(a)) else {
        return false;
    };
    program == name
        || program.starts_with(&format!("{name}-"))
        || (matches!(program, "node" | "bun" | "deno")
            && argv.get(1).is_some_and(|script| script.contains(package)))
}
