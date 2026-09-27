//! Provider-neutral transcript data and bounded JSONL reading.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// Bytes read from the start of a transcript (for the opening prompt).
pub const HEAD_BYTES: u64 = 256 * 1024;
/// Bytes read from the end of a transcript (recent context matters most).
pub const TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// Assistant messages kept per transcript (the most recent ones).
pub const MAX_ASSISTANT: usize = 40;
/// Characters kept per message; longer messages are cut.
pub const MAX_MESSAGE_CHARS: usize = 8 * 1024;

/// What a transcript says, in the order it was said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    /// Summaries of the conversation (e.g. Claude Code's `aiTitle`), oldest first.
    pub titles: Vec<String>,
    /// Messages the human typed, oldest first.
    pub user: Vec<String>,
    /// Assistant prose (no tool calls), oldest first.
    pub assistant: Vec<String>,
    /// When the transcript file was last written.
    pub modified: Option<SystemTime>,
}

impl Transcript {
    /// Record a title, skipping blanks and immediate repeats.
    pub fn push_title(&mut self, title: &str) {
        let title = title.trim();
        if !title.is_empty() && self.titles.last().is_none_or(|t| t != title) {
            self.titles.push(title.to_string());
        }
    }

    /// Record a human prompt, skipping injected noise.
    pub fn push_user(&mut self, text: &str) {
        if !is_noise(text) {
            self.user.push(cap(text));
        }
    }

    /// Record assistant prose.
    pub fn push_assistant(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.assistant.push(cap(text));
        }
    }

    /// Drop all but the most recent assistant messages.
    pub fn trim(&mut self) {
        let n = self.assistant.len();
        if n > MAX_ASSISTANT {
            self.assistant.drain(..n - MAX_ASSISTANT);
        }
    }

    /// The most recent title.
    pub fn latest_title(&self) -> Option<&str> {
        self.titles.last().map(String::as_str)
    }
}

/// Text that looks like a prompt but was injected by the harness: command
/// wrappers (`<command-name>`, `<environment_context>`), caveats, AGENTS.md
/// dumps, interruption markers.
pub fn is_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with('<')
        || t.starts_with("# AGENTS.md")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
}

fn cap(text: &str) -> String {
    match text.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((i, _)) => text[..i].to_string(),
        None => text.to_string(),
    }
}

/// Call `line` for every complete line in the head and tail of a JSONL file,
/// reading at most `head + tail` bytes. Lines cut at a chunk boundary reach
/// the callback truncated; parsers must tolerate invalid JSON.
pub fn for_each_line(
    path: &Path,
    head: u64,
    tail: u64,
    mut line: impl FnMut(&str),
) -> io::Result<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut chunks = Vec::new();
    if size <= head + tail {
        let mut all = Vec::with_capacity(size as usize);
        file.read_to_end(&mut all)?;
        chunks.push(all);
    } else {
        let mut first = vec![0; head as usize];
        file.read_exact(&mut first)?;
        chunks.push(first);
        file.seek(SeekFrom::Start(size - tail))?;
        let mut last = Vec::with_capacity(tail as usize);
        file.read_to_end(&mut last)?;
        chunks.push(last);
    }
    for chunk in &chunks {
        for raw in chunk.split(|b| *b == b'\n') {
            if let Ok(text) = std::str::from_utf8(raw) {
                let text = text.trim();
                if !text.is_empty() {
                    line(text);
                }
            }
        }
    }
    Ok(())
}

/// Text of message content that is either a plain string or an array of
/// `{"type": kind, "text": ...}` items.
pub fn content_texts<'a>(content: &'a serde_json::Value, kind: &str) -> Vec<&'a str> {
    match content {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some(kind))
            .filter_map(|item| item.get("text")?.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_head_and_tail_and_tolerates_cut_lines() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for i in 0..100 {
            writeln!(file, "{{\"n\":\"{i:03}\"}}").unwrap();
        }
        // Each line is 12 bytes; take 30 from the head and 30 from the tail.
        let mut lines = Vec::new();
        for_each_line(file.path(), 30, 30, |l| lines.push(l.to_string())).unwrap();
        assert_eq!(lines.first().unwrap(), r#"{"n":"000"}"#);
        assert_eq!(lines.last().unwrap(), r#"{"n":"099"}"#);
        let whole: Vec<_> = lines
            .iter()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
            .collect();
        assert_eq!(whole.len(), 4, "two whole lines from each end: {lines:?}");
    }

    #[test]
    fn small_files_are_read_whole() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "a\nb\n\nc").unwrap();
        let mut lines = Vec::new();
        for_each_line(file.path(), HEAD_BYTES, TAIL_BYTES, |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(lines, ["a", "b", "c"]);
    }

    #[test]
    fn noise_and_dedup() {
        let mut t = Transcript::default();
        t.push_user("<command-name>/clear</command-name>");
        t.push_user("  ");
        t.push_user("Caveat: the messages below");
        t.push_user("# AGENTS.md instructions");
        t.push_user("refactor the parser");
        t.push_title("Parser refactor");
        t.push_title("Parser refactor");
        t.push_title("Parser refactor and tests");
        assert_eq!(t.user, ["refactor the parser"]);
        assert_eq!(t.titles.len(), 2);
        assert_eq!(t.latest_title(), Some("Parser refactor and tests"));
    }

    #[test]
    fn trims_assistant_history() {
        let mut t = Transcript::default();
        for i in 0..(MAX_ASSISTANT + 5) {
            t.push_assistant(&format!("reply {i}"));
        }
        t.trim();
        assert_eq!(t.assistant.len(), MAX_ASSISTANT);
        assert_eq!(t.assistant[0], "reply 5");
    }

    #[test]
    fn content_texts_handles_both_shapes() {
        let s = serde_json::json!("hello");
        assert_eq!(content_texts(&s, "text"), ["hello"]);
        let a = serde_json::json!([
            {"type": "text", "text": "one"},
            {"type": "tool_result", "content": "skip"},
            {"type": "text", "text": "two"}
        ]);
        assert_eq!(content_texts(&a, "text"), ["one", "two"]);
        assert!(content_texts(&serde_json::json!(null), "text").is_empty());
    }
}
