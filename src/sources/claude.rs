//! Claude Code.
//!
//! Each running Claude Code writes `<config>/sessions/<pid>.json` naming its
//! `sessionId`; the transcript is `<config>/projects/<project>/<sessionId>.jsonl`.
//! Without that file, a `--resume <id>` / `--session-id <id>` argument names
//! the session.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{AgentSource, Candidate, runs};
use crate::procs::ProcessTable;
use crate::transcript::{Transcript, content_texts};

/// Claude Code transcripts under a config dir (`~/.claude`).
#[derive(Debug, Clone)]
pub struct Claude {
    dir: PathBuf,
}

impl Claude {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.into() }
    }

    fn transcript(&self, session_id: &str) -> Option<PathBuf> {
        let name = format!("{session_id}.jsonl");
        fs::read_dir(self.dir.join("projects"))
            .ok()?
            .flatten()
            .map(|project| project.path().join(&name))
            .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
            .max()
            .map(|(_, path)| path)
    }

    /// The session a live process writes, per `<config>/sessions/<pid>.json`.
    fn session_of_pid(&self, pid: u32, procs: &dyn ProcessTable) -> Option<String> {
        let file = self.dir.join("sessions").join(format!("{pid}.json"));
        let meta: SessionMeta = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
        // A stale file whose pid now belongs to another process.
        if let (Some(want), Some(have)) = (&meta.proc_start, procs.start_time(meta.pid))
            && *want != have
        {
            return None;
        }
        (meta.pid == pid).then_some(meta.session_id)
    }
}

/// The session named on a Claude Code command line.
pub fn session_arg(argv: &[String]) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        for flag in ["--resume", "--session-id"] {
            if let Some(value) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
                return Some(value.to_string());
            }
        }
        if matches!(arg.as_str(), "--resume" | "-r" | "--session-id") {
            return args.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMeta {
    pid: u32,
    session_id: String,
    #[serde(default)]
    proc_start: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_compact_summary: bool,
    #[serde(default)]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Value,
}

impl AgentSource for Claude {
    fn kind(&self) -> &str {
        "claude"
    }

    fn locate(&self, procs: &dyn ProcessTable) -> Vec<Candidate> {
        let Ok(entries) = fs::read_dir(self.dir.join("sessions")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let pid: u32 = e.path().file_stem()?.to_str()?.parse().ok()?;
                let session = self.session_of_pid(pid, procs)?;
                let pane_id = procs.pane_of(pid)?;
                let path = self.transcript(&session)?;
                Some(Candidate { pid, pane_id, path })
            })
            .collect()
    }

    fn is_agent(&self, argv: &[String]) -> bool {
        runs(argv, "claude", "claude-code")
    }

    fn transcript_of_process(&self, pid: u32, procs: &dyn ProcessTable) -> Option<PathBuf> {
        let session = self.session_of_pid(pid, procs).or_else(|| {
            let argv = procs.cmdline(pid);
            self.is_agent(&argv).then(|| session_arg(&argv)).flatten()
        })?;
        self.transcript(&session)
    }

    fn transcript_of_session(&self, session_id: &str) -> Option<PathBuf> {
        self.transcript(session_id)
    }

    fn parse_line(&self, line: &str, out: &mut Transcript) {
        let Ok(row) = serde_json::from_str::<Row>(line) else {
            return;
        };
        let content = row.message.map(|m| m.content).unwrap_or_default();
        match row.kind.as_deref() {
            Some("ai-title") => {
                if let Some(title) = &row.ai_title {
                    out.push_title(title);
                }
            }
            Some("user") if row.is_compact_summary => {
                // A summary of earlier conversation: context, not a prompt.
                content_texts(&content, "text")
                    .into_iter()
                    .for_each(|t| out.push_assistant(t));
            }
            Some("user") if !row.is_meta => content_texts(&content, "text")
                .into_iter()
                .for_each(|t| out.push_user(t)),
            Some("assistant") => content_texts(&content, "text")
                .into_iter()
                .for_each(|t| out.push_assistant(t)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{ProcFs, fake};

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude.jsonl");

    fn parse(text: &str) -> Transcript {
        let mut t = Transcript::default();
        for line in text.lines() {
            Claude::new(Path::new("/")).parse_line(line, &mut t);
        }
        t
    }

    #[test]
    fn parses_prompts_replies_and_titles() {
        let t = parse(FIXTURE);
        assert_eq!(
            t.user,
            [
                "Let's refactor the config parser so it handles nested tables",
                "Also add tests for the empty-file case",
            ]
        );
        assert_eq!(t.titles, ["Refactor config parser for nested tables"]);
        assert_eq!(
            t.assistant,
            [
                "I'll start by reading the parser module.",
                "The parser now recurses into nested tables.",
            ]
        );
    }

    #[test]
    fn tolerates_garbage_and_cut_lines() {
        let t = parse("not json\n{\"type\":\"user\",\"message\":{\"content\":\"cut");
        assert_eq!(t, Transcript::default());
    }

    #[test]
    fn locates_transcripts_of_live_processes() {
        let tmp = tempfile::tempdir().unwrap();
        let (claude, proc) = (tmp.path().join("claude"), tmp.path().join("proc"));
        fs::create_dir_all(claude.join("sessions")).unwrap();
        fs::create_dir_all(claude.join("projects/-some-project")).unwrap();
        let transcript = claude.join("projects/-some-project/sess-a.jsonl");
        fs::write(&transcript, FIXTURE).unwrap();
        // Live, in a pane; start time matches the fake procfs (1000 + pid).
        fs::write(
            claude.join("sessions/10.json"),
            r#"{"pid":10,"sessionId":"sess-a","procStart":"1010"}"#,
        )
        .unwrap();
        // Stale: pid 11 was reused by another process.
        fs::write(
            claude.join("sessions/11.json"),
            r#"{"pid":11,"sessionId":"sess-a","procStart":"99"}"#,
        )
        .unwrap();
        // Not in a Herdr pane.
        fs::write(
            claude.join("sessions/12.json"),
            r#"{"pid":12,"sessionId":"sess-a"}"#,
        )
        .unwrap();
        // Transcript missing.
        fs::write(
            claude.join("sessions/13.json"),
            r#"{"pid":13,"sessionId":"sess-missing"}"#,
        )
        .unwrap();
        fs::write(claude.join("sessions/10.key"), "ignored").unwrap();
        fake::process(&proc, 10, 1, &[("HERDR_PANE_ID", "p_a")], &[]);
        fake::process(&proc, 11, 1, &[("HERDR_PANE_ID", "p_b")], &[]);
        fake::process(&proc, 12, 1, &[], &[]);
        fake::process(&proc, 13, 1, &[("HERDR_PANE_ID", "p_c")], &[]);

        let found = Claude::new(&claude).locate(&ProcFs::at(&proc));
        assert_eq!(
            found,
            [Candidate {
                pid: 10,
                pane_id: "p_a".into(),
                path: transcript.clone()
            }]
        );

        // Per process: the sessions file, else the command line.
        let source = Claude::new(&claude);
        let procs = ProcFs::at(&proc);
        assert_eq!(
            source.transcript_of_process(10, &procs),
            Some(transcript.clone())
        );
        assert_eq!(source.transcript_of_process(11, &procs), None);
        fake::process(&proc, 14, 1, &[], &[]);
        fake::cmdline(
            &proc,
            14,
            &["/nix/store/x/bin/claude", "--resume", "sess-a"],
        );
        assert_eq!(
            source.transcript_of_process(14, &procs),
            Some(transcript.clone())
        );
        fake::process(&proc, 15, 1, &[], &[]);
        fake::cmdline(&proc, 15, &["vim", "--resume", "sess-a"]);
        assert_eq!(source.transcript_of_process(15, &procs), None);
        assert_eq!(source.transcript_of_session("sess-a"), Some(transcript));
        assert_eq!(source.transcript_of_session("nope"), None);
    }

    #[test]
    fn reads_session_arguments() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            session_arg(&argv(&["claude", "--resume", "a"])).as_deref(),
            Some("a")
        );
        assert_eq!(
            session_arg(&argv(&["claude", "-x", "--session-id=b"])).as_deref(),
            Some("b")
        );
        assert_eq!(
            session_arg(&argv(&["claude", "-r", "c"])).as_deref(),
            Some("c")
        );
        assert_eq!(session_arg(&argv(&["claude", "--resume"])), None);
        assert_eq!(session_arg(&argv(&["claude", "--resume", "--model"])), None);
        assert_eq!(session_arg(&argv(&["claude"])), None);
        let c = Claude::new(Path::new("/"));
        assert!(c.is_agent(&argv(&["/nix/store/x/libexec/claude", "--resume", "a"])));
        assert!(c.is_agent(&argv(&[
            "node",
            "/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        ])));
        assert!(!c.is_agent(&argv(&["bash", "-c", "claude"])));
        assert!(!c.is_agent(&[]));
    }
}
