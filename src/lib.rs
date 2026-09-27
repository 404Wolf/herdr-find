//! Find a running Herdr agent session by what it's about, and focus its pane.
//!
//! The pipeline: a [`herdr::Multiplexer`] lists the agent panes; each
//! [`sources::AgentSource`] maps its agent's live processes to panes (via
//! [`procs::ProcessTable`]) and transcripts, and parses them; a
//! [`rank::Ranker`] orders the sessions against a description; the
//! [`finder::Finder`] ties it together, and [`mcp`] serves it over MCP.

pub mod config;
pub mod finder;
pub mod herdr;
pub mod mcp;
pub mod procs;
pub mod rank;
pub mod sources;
pub mod transcript;
