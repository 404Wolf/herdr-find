//! Claude Code.
//!
//! Each running Claude Code writes `<config>/sessions/<pid>.json` naming its
//! `sessionId`; the transcript is `<config>/projects/<project>/<sessionId>.jsonl`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{AgentSource, Candidate};
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
                let meta: SessionMeta = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
                // A stale file whose pid now belongs to another process.
                if let (Some(want), Some(have)) = (&meta.proc_start, procs.start_time(meta.pid))
                    && *want != have
                {
                    return None;
                }
                let pane_id = procs.pane_of(meta.pid)?;
                let path = self.transcript(&meta.session_id)?;
                Some(Candidate {
                    pid: meta.pid,
                    pane_id,
                    path,
                })
            })
            .collect()
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
                path: transcript
            }]
        );
    }
}
