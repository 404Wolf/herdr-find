//! Ranking sessions against a free-text description.

use std::collections::HashSet;

use crate::finder::Session;

/// A session's rank for a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// Index into the ranked slice of sessions.
    pub index: usize,
    /// Higher is better; zero means no match.
    pub score: f64,
    /// The passage that best matches the query, if any.
    pub snippet: Option<String>,
}

/// Orders sessions by relevance to a description.
pub trait Ranker: Send + Sync {
    /// Sessions that match `query` at all, best first.
    fn rank(&self, query: &str, sessions: &[Session]) -> Vec<Ranked>;
}

/// Weighted keyword matching with IDF, light stemming and stopwords.
///
/// Each query word earns the weights of the fields it appears in: titles
/// (pane title, conversation summaries) count most, then workspace/tab/cwd
/// and the human's prompts, then recent assistant replies. Words that
/// appear in every session count for less (IDF), and sessions covering more
/// of the query rank higher.
#[derive(Debug, Clone)]
pub struct KeywordRanker {
    pub title_weight: f64,
    pub place_weight: f64,
    pub prompt_weight: f64,
    pub reply_weight: f64,
}

impl Default for KeywordRanker {
    fn default() -> Self {
        Self {
            title_weight: 4.0,
            place_weight: 2.0,
            prompt_weight: 2.0,
            reply_weight: 1.0,
        }
    }
}

/// Words that say nothing about a session's topic.
const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "been",
    "but",
    "by",
    "can",
    "chat",
    "chatted",
    "chatting",
    "conversation",
    "could",
    "did",
    "discussed",
    "discussing",
    "do",
    "does",
    "doing",
    "find",
    "for",
    "from",
    "go",
    "had",
    "has",
    "have",
    "herdr",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "let",
    "me",
    "my",
    "of",
    "ll",
    "on",
    "one",
    "open",
    "or",
    "our",
    "pane",
    "re",
    "session",
    "show",
    "so",
    "some",
    "switch",
    "take",
    "talked",
    "talking",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "this",
    "to",
    "up",
    "us",
    "ve",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "window",
    "with",
    "would",
    "you",
    "your",
];

/// Lowercased words of `text` with their byte offsets, stopwords removed.
fn words(text: &str) -> impl Iterator<Item = (usize, String)> + '_ {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text[s..i].to_lowercase();
                if word.chars().count() > 1 && !STOPWORDS.contains(&word.as_str()) {
                    out.push((s, word));
                }
                start = None;
            }
            _ => {}
        }
    }
    out.into_iter()
}

/// Strip common English suffixes: `enums` → `enum`, `making` → `mak`.
fn stem(word: &str) -> String {
    let mut w = word;
    for (suffix, keep) in [
        ("ies", 3),
        ("ing", 3),
        ("ed", 3),
        ("es", 3),
        ("ly", 4),
        ("s", 3),
    ] {
        if w.len() >= suffix.len() + keep
            && w.ends_with(suffix)
            && w.is_char_boundary(w.len() - suffix.len())
        {
            w = &w[..w.len() - suffix.len()];
            break;
        }
    }
    let w = w.strip_suffix('e').filter(|s| s.len() >= 3).unwrap_or(w);
    w.to_string()
}

/// Forms of a word that count as the same word: itself, its stem, and for
/// long words a six-letter prefix (`exhaustive` ~ `exhaustively`).
fn keys(word: &str) -> Vec<String> {
    let stem = stem(word);
    let mut keys = vec![word.to_string()];
    if stem.chars().count() > 6 {
        keys.push(stem.chars().take(6).collect());
    }
    keys.push(stem);
    keys
}

/// Every key of every word in some texts.
fn vocabulary<'a>(texts: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    texts
        .into_iter()
        .flat_map(words)
        .flat_map(|(_, w)| keys(&w))
        .collect()
}

struct Fields {
    weighted: Vec<(f64, HashSet<String>)>,
}

impl KeywordRanker {
    fn fields(&self, s: &Session) -> Fields {
        let t = s.transcript.as_ref();
        let titles = std::iter::once(s.pane.title.as_str()).chain(
            t.into_iter()
                .flat_map(|t| t.titles.iter().map(String::as_str)),
        );
        let place = [
            s.workspace.as_str(),
            s.tab.as_str(),
            s.pane.cwd.as_str(),
            s.pane.agent.as_str(),
        ];
        let prompts = t
            .into_iter()
            .flat_map(|t| t.user.iter().map(String::as_str));
        let replies = t
            .into_iter()
            .flat_map(|t| t.assistant.iter().map(String::as_str));
        Fields {
            weighted: vec![
                (self.title_weight, vocabulary(titles)),
                (self.place_weight, vocabulary(place)),
                (self.prompt_weight, vocabulary(prompts)),
                (self.reply_weight, vocabulary(replies)),
            ],
        }
    }
}

fn hits(query: &[String], vocab: &HashSet<String>) -> bool {
    query.iter().any(|k| vocab.contains(k))
}

impl Ranker for KeywordRanker {
    fn rank(&self, query: &str, sessions: &[Session]) -> Vec<Ranked> {
        let mut seen = HashSet::new();
        let query: Vec<Vec<String>> = words(query)
            .filter(|(_, w)| seen.insert(w.clone()))
            .map(|(_, w)| keys(&w))
            .collect();
        if query.is_empty() {
            return Vec::new();
        }
        let fields: Vec<Fields> = sessions.iter().map(|s| self.fields(s)).collect();
        let n = sessions.len() as f64;
        let idf: Vec<f64> = query
            .iter()
            .map(|q| {
                let df = fields
                    .iter()
                    .filter(|f| f.weighted.iter().any(|(_, v)| hits(q, v)))
                    .count() as f64;
                (1.0 + n / df.max(1.0)).ln()
            })
            .collect();
        let total_idf: f64 = idf.iter().sum();

        let mut ranked: Vec<Ranked> = fields
            .iter()
            .enumerate()
            .filter_map(|(index, f)| {
                let (mut points, mut covered) = (0.0, 0.0);
                for (q, idf) in query.iter().zip(&idf) {
                    let weight: f64 = f
                        .weighted
                        .iter()
                        .filter(|(_, v)| hits(q, v))
                        .map(|(w, _)| w)
                        .sum();
                    if weight > 0.0 {
                        points += weight * idf;
                        covered += idf;
                    }
                }
                (points > 0.0).then(|| Ranked {
                    index,
                    score: (points / total_idf) * (covered / total_idf),
                    snippet: snippet(&sessions[index], &query),
                })
            })
            .collect();
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
        ranked
    }
}

/// The prompt (else reply, else title) covering the most query words, the
/// most recent winning ties, cut to a window around the first match.
fn snippet(s: &Session, query: &[Vec<String>]) -> Option<String> {
    let t = s.transcript.as_ref()?;
    let coverage = |text: &str| {
        let vocab = vocabulary([text]);
        query.iter().filter(|q| hits(q, &vocab)).count()
    };
    [&t.user, &t.assistant, &t.titles]
        .into_iter()
        .find_map(|texts| {
            let (best, text) = texts
                .iter()
                .map(|m| (coverage(m), m))
                .max_by_key(|(c, _)| *c)?;
            (best > 0).then(|| around_match(text, query, SNIPPET_CHARS))
        })
}

/// Characters in a matching snippet.
pub const SNIPPET_CHARS: usize = 240;

/// `text` with whitespace collapsed, cut to `max` characters around the first
/// word matching the query.
fn around_match(text: &str, query: &[Vec<String>], max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let at = words(&text)
        .find(|(_, w)| {
            let k = keys(w);
            query.iter().any(|q| q.iter().any(|x| k.contains(x)))
        })
        .map_or(0, |(i, _)| text[..i].chars().count());
    let len = text.chars().count();
    if len <= max {
        return text;
    }
    let start = at.saturating_sub(max / 4).min(len - max);
    let body: String = text.chars().skip(start).take(max - 2).collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if start + max - 2 < len { "…" } else { "" };
    format!("{prefix}{}{suffix}", body.trim())
}

/// `text` with whitespace collapsed, cut to `max` characters.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let body: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", body.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentPane;
    use crate::transcript::Transcript;

    fn session(title: &str, workspace: &str, user: &[&str], assistant: &[&str]) -> Session {
        Session {
            pane: AgentPane {
                pane_id: format!("p_{workspace}"),
                agent: "claude".into(),
                title: title.into(),
                ..Default::default()
            },
            workspace: workspace.into(),
            transcript: Some(Transcript {
                user: user.iter().map(|s| s.to_string()).collect(),
                assistant: assistant.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn stems_and_keys() {
        assert_eq!(stem("enums"), "enum");
        assert_eq!(stem("making"), "mak");
        assert_eq!(stem("make"), "mak");
        assert_eq!(stem("caches"), "cach");
        assert_eq!(stem("exhaustively"), "exhaustiv");
        assert!(keys("exhaustive").contains(&"exhaus".to_string()));
        assert!(keys("exhaustively").contains(&"exhaus".to_string()));
        let q: Vec<_> = words("Take me to the herdr window where we're chatting about Enums")
            .map(|(_, w)| w)
            .collect();
        assert_eq!(q, ["enums"]);
    }

    #[test]
    fn ranks_by_topic() {
        let sessions = [
            session(
                "Thumbnail cache",
                "images",
                &["Speed up the image thumbnail cache"],
                &["Added an LRU cache."],
            ),
            session(
                "Parser work",
                "config",
                &["Let's refactor the config parser for nested tables"],
                &["The parser recurses now."],
            ),
            session("idle shell", "misc", &["hello"], &[]),
        ];
        let r = KeywordRanker::default();
        let ranked = r.rank(
            "the window where we're refactoring the parser to handle nested tables",
            &sessions,
        );
        assert_eq!(ranked[0].index, 1);
        assert!(ranked.iter().all(|m| m.index != 2), "{ranked:?}");
        assert_eq!(
            ranked[0].snippet.as_deref(),
            Some("Let's refactor the config parser for nested tables")
        );
        let ranked = r.rank("thumbnails caching", &sessions);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].index, 0);
        assert!(r.rank("take me to the herdr window", &sessions).is_empty());
    }

    #[test]
    fn coverage_beats_one_repeated_word() {
        let sessions = [
            // "parser" everywhere, nothing else.
            session("parser", "parser", &["parser"], &["parser"]),
            // All three query words, prompts only.
            session(
                "misc",
                "misc",
                &["make the parser errors exhaustive for enums"],
                &[],
            ),
        ];
        let ranked = KeywordRanker::default().rank("exhaustive parser enums", &sessions);
        assert_eq!(ranked[0].index, 1, "{ranked:?}");
    }

    #[test]
    fn titles_outweigh_replies() {
        let sessions = [
            session("misc", "a", &[], &["we might touch the scheduler later"]),
            session("Scheduler rewrite", "b", &[], &[]),
        ];
        let ranked = KeywordRanker::default().rank("scheduler", &sessions);
        assert_eq!(ranked[0].index, 1);
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn snippets_are_cut_around_the_match() {
        let long = format!(
            "{} the scheduler is here {}",
            "x ".repeat(300),
            "y ".repeat(300)
        );
        let q = vec![keys("scheduler")];
        let s = around_match(&long, &q, 60);
        assert!(s.contains("scheduler"), "{s}");
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.chars().count() <= 60);
        assert_eq!(truncate("a  b\n c", 10), "a b c");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
