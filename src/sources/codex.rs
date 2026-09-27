//! Codex.
//!
//! A running Codex keeps its current rollout file open, so the transcript is
//! whichever `<home>/sessions/**/rollout-*.jsonl` the process has open. As a
//! fallback, Codex also holds `<home>/thread-writer-locks/<thread id>.lock`,
//! and the rollout's file name ends in that thread id.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{AgentSource, Candidate};
use crate::procs::{ProcessTable, is_under};
use crate::transcript::{Transcript, content_texts};

/// Codex rollouts under a Codex home (`~/.codex`).
#[derive(Debug, Clone)]
pub struct Codex {
    sessions: PathBuf,
    locks: PathBuf,
}

impl Codex {
    pub fn new(home: &Path) -> Self {
        Self {
            sessions: home.join("sessions"),
            locks: home.join("thread-writer-locks"),
        }
    }

    /// The rollout of a thread, found by walking the sessions tree.
    fn rollout_of_thread(&self, thread: &str) -> Option<PathBuf> {
        let suffix = format!("-{thread}.jsonl");
        let mut dirs = vec![self.sessions.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).ok()?.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => dirs.push(path),
                    Ok(_)
                        if self.is_rollout(&path)
                            && path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.ends_with(&suffix)) =>
                    {
                        return Some(path);
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// The transcript of one process: its newest open rollout, else the
    /// rollout of the thread it holds a writer lock for.
    fn transcript_of(&self, open: Vec<PathBuf>) -> Option<PathBuf> {
        let newest = open
            .iter()
            .filter(|p| self.is_rollout(p))
            .filter_map(|p| Some((fs::metadata(p).ok()?.modified().ok()?, p)))
            .max()
            .map(|(_, p)| p.clone());
        newest.or_else(|| {
            open.iter()
                .filter(|p| p.extension().is_some_and(|x| x == "lock") && is_under(p, &self.locks))
                .filter_map(|p| p.file_stem()?.to_str())
                .find_map(|thread| self.rollout_of_thread(thread))
        })
    }

    fn is_rollout(&self, path: &Path) -> bool {
        path.extension().is_some_and(|x| x == "jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
            && is_under(path, &self.sessions)
    }
}

#[derive(Deserialize)]
struct Row {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Value,
}

impl AgentSource for Codex {
    fn kind(&self) -> &str {
        "codex"
    }

    fn locate(&self, procs: &dyn ProcessTable) -> Vec<Candidate> {
        if !self.sessions.is_dir() {
            return Vec::new();
        }
        procs
            .pids()
            .into_iter()
            .filter_map(|pid| {
                let path = self.transcript_of(procs.open_files(pid))?;
                Some(Candidate {
                    pid,
                    pane_id: procs.pane_of(pid)?,
                    path,
                })
            })
            .collect()
    }

    fn parse_line(&self, line: &str, out: &mut Transcript) {
        let Ok(Row {
            kind,
            payload: Some(p),
        }) = serde_json::from_str::<Row>(line)
        else {
            return;
        };
        if kind.as_deref() != Some("response_item") || p.kind.as_deref() != Some("message") {
            return;
        }
        match p.role.as_deref() {
            Some("user") => content_texts(&p.content, "input_text")
                .into_iter()
                .for_each(|t| out.push_user(t)),
            Some("assistant") => content_texts(&p.content, "output_text")
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

    const FIXTURE: &str = include_str!("../../tests/fixtures/codex.jsonl");

    #[test]
    fn parses_prompts_and_replies() {
        let mut t = Transcript::default();
        for line in FIXTURE.lines() {
            Codex::new(Path::new("/")).parse_line(line, &mut t);
        }
        assert_eq!(
            t.user,
            [
                "Speed up the image thumbnail cache",
                "Benchmark it against the old version"
            ]
        );
        assert_eq!(
            t.assistant,
            ["The thumbnail cache now uses an LRU eviction policy."]
        );
        assert!(t.titles.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn locates_open_rollouts() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, proc) = (tmp.path().join("codex"), tmp.path().join("proc"));
        let day = home.join("sessions/2026/01/01");
        fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-01-01T00-00-00-abc.jsonl");
        let other = day.join("history.jsonl");
        let outside = tmp.path().join("rollout-elsewhere.jsonl");
        for f in [&rollout, &other, &outside] {
            fs::write(f, FIXTURE).unwrap();
        }
        fake::process(
            &proc,
            20,
            1,
            &[("HERDR_PANE_ID", "p_x")],
            &[&other, &rollout],
        );
        fake::process(&proc, 21, 1, &[("HERDR_PANE_ID", "p_y")], &[&outside]);
        fake::process(&proc, 22, 1, &[], &[&rollout]);
        // Holds only the thread's writer lock.
        fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
        let lock = home.join("thread-writer-locks/def.lock");
        let locked = day.join("rollout-2026-01-01T00-00-01-def.jsonl");
        fs::write(&lock, "").unwrap();
        fs::write(&locked, FIXTURE).unwrap();
        fake::process(&proc, 23, 1, &[("HERDR_PANE_ID", "p_z")], &[&lock]);

        let mut found = Codex::new(&home).locate(&ProcFs::at(&proc));
        found.sort_by_key(|c| c.pid);
        assert_eq!(
            found,
            [
                Candidate {
                    pid: 20,
                    pane_id: "p_x".into(),
                    path: rollout
                },
                Candidate {
                    pid: 23,
                    pane_id: "p_z".into(),
                    path: locked
                }
            ]
        );
    }
}
