//! The pipeline: list agent panes, attach transcripts, rank, summarise.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::thread;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::config::Config;
use crate::herdr::{AgentPane, HerdrCli, Multiplexer, validate_pane_id};
use crate::procs::{ProcFs, ProcessTable};
use crate::rank::{KeywordRanker, Ranker, truncate};
use crate::sources::{AgentSource, Candidate, default_sources};
use crate::transcript::{HEAD_BYTES, TAIL_BYTES, Transcript, for_each_line};

/// A running agent pane with whatever we know about its conversation.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub pane: AgentPane,
    pub workspace: String,
    pub tab: String,
    /// The pane this process was started from (the calling agent).
    pub is_current: bool,
    pub transcript_path: Option<PathBuf>,
    pub transcript: Option<Transcript>,
}

/// A session as reported to callers: enough to recognise it, never the
/// whole transcript.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct SessionSummary {
    /// Pass this to `focus_herdr_pane`.
    pub pane_id: String,
    /// Agent kind, e.g. `claude` or `codex`.
    pub agent: String,
    /// `idle`, `working`, `blocked`, `done` or `unknown`.
    pub status: String,
    /// Terminal title of the pane.
    pub title: String,
    /// Latest summary of the conversation, when the agent writes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub workspace: String,
    pub tab: String,
    pub cwd: String,
    /// Whether the pane has focus in the Herdr UI right now.
    pub focused: bool,
    /// Whether this is the pane of the agent calling this tool.
    pub is_current_pane: bool,
    /// Whether the conversation transcript could be found and read.
    pub has_transcript: bool,
    /// Seconds since the transcript was last written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_seconds: Option<u64>,
    /// The first thing the human asked.
    pub first_prompt: String,
    /// The last few things the human asked, oldest first.
    pub recent_prompts: Vec<String>,
}

/// A ranked search hit.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct Match {
    #[serde(flatten)]
    pub session: SessionSummary,
    /// Relevance; only meaningful relative to the other matches.
    pub score: f64,
    /// The passage that matched best.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Characters kept of the first prompt.
const FIRST_PROMPT_CHARS: usize = 240;
/// Characters kept of each recent prompt.
const RECENT_PROMPT_CHARS: usize = 160;
/// Recent prompts reported per session.
const RECENT_PROMPTS: usize = 3;

/// Finds agent sessions by topic and focuses their panes.
pub struct Finder {
    pub mux: Box<dyn Multiplexer>,
    pub procs: Box<dyn ProcessTable>,
    pub sources: Vec<Box<dyn AgentSource>>,
    pub ranker: Box<dyn Ranker>,
    /// The caller's own pane, flagged and left out of search results.
    pub current_pane: Option<String>,
}

impl Finder {
    /// The default setup: Herdr CLI, system procfs, all agent sources,
    /// keyword ranking.
    pub fn from_config(config: &Config) -> Self {
        Self {
            mux: Box::new(HerdrCli::new(&config.herdr_bin)),
            procs: match &config.proc_dir {
                Some(dir) => Box::new(ProcFs::at(dir)),
                None => Box::new(ProcFs::system()),
            },
            sources: default_sources(config),
            ranker: Box::new(KeywordRanker::default()),
            current_pane: config.current_pane.clone(),
        }
    }

    /// Notes about degraded results (e.g. no transcripts on this platform).
    pub fn warnings(&self) -> Vec<String> {
        if self.procs.supported() {
            Vec::new()
        } else {
            vec![
                "Mapping panes to transcripts needs Linux /proc; searching titles, workspaces, tabs and directories only.".into(),
            ]
        }
    }

    /// Every running agent session, transcripts attached where found.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let panes = self.mux.agents()?;
        // Labels are nice to have; a failure here shouldn't sink the search.
        let workspaces = self.mux.workspaces().unwrap_or_default();
        let tabs = self.mux.tabs().unwrap_or_default();
        let located = self.locate(&panes);

        let mut sessions: Vec<Session> = panes
            .into_iter()
            .map(|pane| {
                let transcript_path = located
                    .get(&(pane.agent.clone(), pane.pane_id.clone()))
                    .cloned();
                Session {
                    workspace: workspaces
                        .get(&pane.workspace_id)
                        .cloned()
                        .unwrap_or_default(),
                    tab: tabs.get(&pane.tab_id).cloned().unwrap_or_default(),
                    is_current: self.current_pane.as_deref() == Some(pane.pane_id.as_str()),
                    transcript_path,
                    transcript: None,
                    pane,
                }
            })
            .collect();

        thread::scope(|scope| {
            for session in &mut sessions {
                let Some(source) = self.source(&session.pane.agent) else {
                    continue;
                };
                let Some(path) = session.transcript_path.clone() else {
                    continue;
                };
                scope.spawn(move || session.transcript = read_transcript(source, &path));
            }
        });
        Ok(sessions)
    }

    fn source(&self, kind: &str) -> Option<&dyn AgentSource> {
        self.sources
            .iter()
            .find(|s| s.kind() == kind)
            .map(|s| s.as_ref())
    }

    /// (agent kind, pane id) → transcript, for the kinds present in `panes`.
    fn locate(&self, panes: &[AgentPane]) -> HashMap<(String, String), PathBuf> {
        let kinds: HashSet<&str> = panes.iter().map(|p| p.agent.as_str()).collect();
        let mut found = HashMap::new();
        for source in self.sources.iter().filter(|s| kinds.contains(s.kind())) {
            for (pane, path) in outermost(source.locate(self.procs.as_ref()), self.procs.as_ref()) {
                found.insert((source.kind().to_string(), pane), path);
            }
        }
        found
    }

    /// Sessions matching `description`, best first, and how many sessions
    /// were searched. The caller's own pane is left out unless
    /// `include_current`.
    pub fn find(
        &self,
        description: &str,
        limit: usize,
        include_current: bool,
    ) -> Result<(Vec<Match>, usize)> {
        let sessions: Vec<Session> = self
            .sessions()?
            .into_iter()
            .filter(|s| include_current || !s.is_current)
            .collect();
        let matches = self
            .ranker
            .rank(description, &sessions)
            .into_iter()
            .take(limit)
            .map(|r| Match {
                session: summarize(&sessions[r.index]),
                score: (r.score * 100.0).round() / 100.0,
                snippet: r.snippet,
            })
            .collect();
        Ok((matches, sessions.len()))
    }

    /// Summaries of every running session.
    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        Ok(self.sessions()?.iter().map(summarize).collect())
    }

    /// Switch the user's view to `pane_id`.
    pub fn focus(&self, pane_id: &str) -> Result<()> {
        validate_pane_id(pane_id)?;
        self.mux.focus(pane_id)
    }
}

/// One transcript per pane: when several processes of a kind share a pane
/// (an agent started from inside another), keep the outermost one.
fn outermost(candidates: Vec<Candidate>, procs: &dyn ProcessTable) -> HashMap<String, PathBuf> {
    let pids: HashSet<u32> = candidates.iter().map(|c| c.pid).collect();
    let has_candidate_ancestor = |pid: u32| {
        let mut seen = HashSet::new();
        let mut cur = procs.parent(pid);
        while let Some(p) = cur {
            if p <= 1 || !seen.insert(p) {
                break;
            }
            if pids.contains(&p) {
                return true;
            }
            cur = procs.parent(p);
        }
        false
    };
    let mut best: HashMap<String, (bool, Option<std::time::SystemTime>, PathBuf)> = HashMap::new();
    for c in candidates {
        let outer = !has_candidate_ancestor(c.pid);
        let modified = std::fs::metadata(&c.path).and_then(|m| m.modified()).ok();
        let entry = (outer, modified, c.path);
        match best.get(&c.pane_id) {
            Some(prev) if (prev.0, prev.1) >= (entry.0, entry.1) => {}
            _ => {
                best.insert(c.pane_id, entry);
            }
        }
    }
    best.into_iter()
        .map(|(pane, (_, _, path))| (pane, path))
        .collect()
}

/// Read a transcript's head and tail; `None` if it can't be read.
fn read_transcript(source: &dyn AgentSource, path: &std::path::Path) -> Option<Transcript> {
    let mut t = Transcript::default();
    for_each_line(path, HEAD_BYTES, TAIL_BYTES, |line| {
        source.parse_line(line, &mut t)
    })
    .ok()?;
    t.trim();
    t.modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    Some(t)
}

fn summarize(s: &Session) -> SessionSummary {
    let t = s.transcript.as_ref();
    let user = t.map(|t| t.user.as_slice()).unwrap_or_default();
    SessionSummary {
        pane_id: s.pane.pane_id.clone(),
        agent: s.pane.agent.clone(),
        status: s.pane.status.clone(),
        title: s.pane.title.clone(),
        summary: t.and_then(|t| t.latest_title()).map(String::from),
        workspace: s.workspace.clone(),
        tab: s.tab.clone(),
        cwd: s.pane.cwd.clone(),
        focused: s.pane.focused,
        is_current_pane: s.is_current,
        has_transcript: t.is_some(),
        idle_seconds: t
            .and_then(|t| t.modified)
            .and_then(|m| m.elapsed().ok())
            .map(|d| d.as_secs()),
        first_prompt: user
            .first()
            .map(|p| truncate(p, FIRST_PROMPT_CHARS))
            .unwrap_or_default(),
        recent_prompts: user[user.len().saturating_sub(RECENT_PROMPTS)..]
            .iter()
            .map(|p| truncate(p, RECENT_PROMPT_CHARS))
            .collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::procs::fake;
    use std::fs;
    use std::sync::{Arc, Mutex};

    /// A multiplexer with canned panes that records focus calls.
    #[derive(Default, Clone)]
    pub struct MockMux {
        pub panes: Vec<AgentPane>,
        pub focused: Arc<Mutex<Vec<String>>>,
        pub fail: bool,
    }

    impl Multiplexer for MockMux {
        fn agents(&self) -> Result<Vec<AgentPane>> {
            if self.fail {
                anyhow::bail!("herdr is not running");
            }
            Ok(self.panes.clone())
        }
        fn workspaces(&self) -> Result<HashMap<String, String>> {
            Ok([
                ("w_1".into(), "parsers".into()),
                ("w_2".into(), "media".into()),
            ]
            .into())
        }
        fn tabs(&self) -> Result<HashMap<String, String>> {
            anyhow::bail!("tabs unavailable")
        }
        fn focus(&self, pane_id: &str) -> Result<()> {
            if !self.panes.iter().any(|p| p.pane_id == pane_id) {
                anyhow::bail!("pane {pane_id} not found");
            }
            self.focused.lock().unwrap().push(pane_id.into());
            Ok(())
        }
    }

    fn pane(id: &str, agent: &str, workspace: &str, title: &str) -> AgentPane {
        AgentPane {
            pane_id: id.into(),
            agent: agent.into(),
            status: "idle".into(),
            workspace_id: workspace.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    /// A Claude pane, a Codex pane, a Claude pane without a transcript and
    /// the caller's own pane, backed by fixture transcripts.
    pub fn setup(tmp: &std::path::Path) -> (Finder, MockMux) {
        let (claude, codex, proc) = (tmp.join("claude"), tmp.join("codex"), tmp.join("proc"));
        fs::create_dir_all(claude.join("sessions")).unwrap();
        fs::create_dir_all(claude.join("projects/-demo")).unwrap();
        fs::write(
            claude.join("projects/-demo/sess-parser.jsonl"),
            include_str!("../tests/fixtures/claude.jsonl"),
        )
        .unwrap();
        fs::write(
            claude.join("projects/-demo/sess-self.jsonl"),
            r#"{"type":"user","message":{"content":"take me to the session about the config parser"}}"#,
        )
        .unwrap();
        fs::write(
            claude.join("sessions/100.json"),
            r#"{"pid":100,"sessionId":"sess-parser"}"#,
        )
        .unwrap();
        fs::write(
            claude.join("sessions/103.json"),
            r#"{"pid":103,"sessionId":"sess-self"}"#,
        )
        .unwrap();
        let day = codex.join("sessions/2026/01/01");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-01-01T00-00-00-x.jsonl");
        fs::write(&rollout, include_str!("../tests/fixtures/codex.jsonl")).unwrap();
        fake::process(&proc, 100, 1, &[("HERDR_PANE_ID", "p_parser")], &[]);
        fake::process(&proc, 101, 1, &[("HERDR_PANE_ID", "p_thumbs")], &[&rollout]);
        // A codex started from inside the codex pane: ignored.
        fake::process(
            &proc,
            105,
            101,
            &[("HERDR_PANE_ID", "p_thumbs")],
            &[&rollout],
        );
        fake::process(&proc, 103, 1, &[("HERDR_PANE_ID", "p_self")], &[]);

        let mux = MockMux {
            panes: vec![
                pane("p_parser", "claude", "w_1", "Parser work"),
                pane("p_thumbs", "codex", "w_2", "codex"),
                pane("p_blank", "claude", "w_2", "Claude Code"),
                pane("p_self", "claude", "w_1", "Finder"),
            ],
            ..Default::default()
        };
        let config = Config {
            claude_dir: claude,
            codex_dir: codex,
            proc_dir: Some(proc),
            herdr_bin: "unused".into(),
            current_pane: Some("p_self".into()),
        };
        let mut finder = Finder::from_config(&config);
        finder.mux = Box::new(mux.clone());
        (finder, mux)
    }

    #[cfg(unix)]
    #[test]
    fn lists_sessions_with_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let (finder, _) = setup(tmp.path());
        let list = finder.list().unwrap();
        assert_eq!(list.len(), 4);
        let by_id = |id: &str| list.iter().find(|s| s.pane_id == id).unwrap();
        let parser = by_id("p_parser");
        assert!(parser.has_transcript);
        assert_eq!(parser.workspace, "parsers");
        assert_eq!(parser.tab, "");
        assert_eq!(
            parser.summary.as_deref(),
            Some("Refactor config parser for nested tables")
        );
        assert_eq!(
            parser.first_prompt,
            "Let's refactor the config parser so it handles nested tables"
        );
        assert_eq!(parser.recent_prompts.len(), 2);
        let thumbs = by_id("p_thumbs");
        assert!(thumbs.has_transcript);
        assert_eq!(thumbs.first_prompt, "Speed up the image thumbnail cache");
        assert!(!by_id("p_blank").has_transcript);
        assert!(by_id("p_self").is_current_pane);
    }

    #[cfg(unix)]
    #[test]
    fn finds_by_topic_and_skips_the_caller() {
        let tmp = tempfile::tempdir().unwrap();
        let (finder, _) = setup(tmp.path());
        let (hits, searched) = finder
            .find("where we refactor the config parser", 5, false)
            .unwrap();
        assert_eq!(searched, 3);
        assert_eq!(hits[0].session.pane_id, "p_parser");
        assert!(hits.iter().all(|m| m.session.pane_id != "p_self"));
        assert!(hits[0].snippet.as_deref().unwrap().contains("parser"));

        let (hits, _) = finder
            .find("thumbnail caching benchmark", 5, false)
            .unwrap();
        assert_eq!(hits[0].session.pane_id, "p_thumbs");

        let (hits, _) = finder.find("config parser", 5, true).unwrap();
        assert!(hits.iter().any(|m| m.session.pane_id == "p_self"));

        assert_eq!(finder.find("config parser", 1, true).unwrap().0.len(), 1);
        assert!(
            finder
                .find("quantum entanglement", 5, false)
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn focuses_through_the_multiplexer() {
        let tmp = tempfile::tempdir().unwrap();
        let (finder, mux) = setup(tmp.path());
        finder.focus("p_thumbs").unwrap();
        assert!(finder.focus("p_missing").is_err());
        assert!(finder.focus("--help").is_err());
        assert_eq!(*mux.focused.lock().unwrap(), ["p_thumbs"]);
    }

    #[test]
    fn multiplexer_errors_surface() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut finder, mut mux) = setup(tmp.path());
        mux.fail = true;
        finder.mux = Box::new(mux);
        assert!(
            finder
                .list()
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
    }

    #[test]
    fn unreadable_transcripts_leave_the_session_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let (finder, _) = setup(tmp.path());
        fs::remove_file(tmp.path().join("claude/projects/-demo/sess-parser.jsonl")).unwrap();
        let list = finder.list().unwrap();
        let parser = list.iter().find(|s| s.pane_id == "p_parser").unwrap();
        assert!(!parser.has_transcript);
        assert_eq!(parser.title, "Parser work");
    }
}
