//! The terminal multiplexer: listing agent panes and focusing one.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// An agent pane as the multiplexer reports it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AgentPane {
    pub pane_id: String,
    /// Agent kind, e.g. `claude` or `codex`.
    #[serde(default)]
    pub agent: String,
    /// `idle`, `working`, `blocked`, `done` or `unknown`.
    #[serde(default, rename = "agent_status")]
    pub status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: String,
    /// Terminal title with spinners and status glyphs removed.
    #[serde(default, rename = "terminal_title_stripped")]
    pub title: String,
    /// Whether this pane has focus in the UI.
    #[serde(default)]
    pub focused: bool,
}

/// What `herdr-find` needs from a terminal multiplexer.
pub trait Multiplexer: Send + Sync {
    /// Every pane running a coding agent.
    fn agents(&self) -> Result<Vec<AgentPane>>;
    /// Workspace id → label.
    fn workspaces(&self) -> Result<HashMap<String, String>>;
    /// Tab id → label.
    fn tabs(&self) -> Result<HashMap<String, String>>;
    /// Switch the user's view to a pane.
    fn focus(&self, pane_id: &str) -> Result<()>;
}

/// Herdr, driven through its CLI.
#[derive(Debug, Clone)]
pub struct HerdrCli {
    bin: String,
    timeout: Duration,
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: T,
}

#[derive(Deserialize)]
struct Agents {
    #[serde(default)]
    agents: Vec<AgentPane>,
}

#[derive(Deserialize)]
struct Workspaces {
    #[serde(default)]
    workspaces: Vec<Labelled>,
}

#[derive(Deserialize)]
struct Tabs {
    #[serde(default)]
    tabs: Vec<Labelled>,
}

#[derive(Deserialize)]
struct Labelled {
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn labels(items: Vec<Labelled>, id: fn(&Labelled) -> &str) -> HashMap<String, String> {
    items
        .into_iter()
        .map(|l| {
            let key = id(&l).to_string();
            let label = l
                .label
                .filter(|s| !s.is_empty())
                .or(l.name)
                .unwrap_or_default();
            (key, label)
        })
        .collect()
}

impl HerdrCli {
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            timeout: Duration::from_secs(15),
        }
    }

    /// Run herdr and return its stdout; errors carry herdr's stderr.
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run `{}`", self.bin))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out = thread::spawn(move || {
            let mut s = String::new();
            stdout.read_to_string(&mut s).map(|_| s)
        });
        let err = thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`herdr {}` timed out", args.join(" "));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let stdout = out
            .join()
            .map_err(|_| anyhow!("stdout reader panicked"))??;
        let stderr = err.join().unwrap_or_default();
        if !status.success() {
            bail!(
                "`herdr {}` failed ({status}): {}",
                args.join(" "),
                stderr.trim()
            );
        }
        Ok(stdout)
    }

    fn json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let out = self.run(args)?;
        let envelope: Envelope<T> = serde_json::from_str(&out)
            .with_context(|| format!("unexpected output from `herdr {}`", args.join(" ")))?;
        Ok(envelope.result)
    }
}

impl Multiplexer for HerdrCli {
    fn agents(&self) -> Result<Vec<AgentPane>> {
        Ok(self.json::<Agents>(&["agent", "list"])?.agents)
    }

    fn workspaces(&self) -> Result<HashMap<String, String>> {
        Ok(labels(
            self.json::<Workspaces>(&["workspace", "list"])?.workspaces,
            |l| &l.workspace_id,
        ))
    }

    fn tabs(&self) -> Result<HashMap<String, String>> {
        Ok(labels(self.json::<Tabs>(&["tab", "list"])?.tabs, |l| {
            &l.tab_id
        }))
    }

    fn focus(&self, pane_id: &str) -> Result<()> {
        validate_pane_id(pane_id)?;
        self.run(&["agent", "focus", pane_id]).map(|_| ())
    }
}

/// Reject pane ids that could be read as flags or are otherwise not ids.
pub fn validate_pane_id(pane_id: &str) -> Result<()> {
    if pane_id.is_empty()
        || pane_id.starts_with('-')
        || pane_id.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("invalid pane id {pane_id:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_herdr_json() {
        let agents: Envelope<Agents> = serde_json::from_str(
            r#"{"id":"1","result":{"type":"agent_list","agents":[
                {"pane_id":"p_1","agent":"claude","agent_status":"idle","cwd":"/w",
                 "workspace_id":"w_1","tab_id":"t_1","terminal_title_stripped":"Parser work",
                 "focused":true,"revision":3}]}}"#,
        )
        .unwrap();
        assert_eq!(
            agents.result.agents,
            [AgentPane {
                pane_id: "p_1".into(),
                agent: "claude".into(),
                status: "idle".into(),
                cwd: "/w".into(),
                workspace_id: "w_1".into(),
                tab_id: "t_1".into(),
                title: "Parser work".into(),
                focused: true,
            }]
        );
        let tabs: Envelope<Tabs> = serde_json::from_str(
            r#"{"id":"2","result":{"tabs":[{"tab_id":"t_1","workspace_id":"w_1","label":"main"},{"tab_id":"t_2","workspace_id":"w_1","label":"","name":"alt"}]}}"#,
        )
        .unwrap();
        let tabs = labels(tabs.result.tabs, |l| &l.tab_id);
        assert_eq!(tabs["t_1"], "main");
        assert_eq!(tabs["t_2"], "alt");
    }

    #[test]
    fn rejects_flag_like_pane_ids() {
        assert!(validate_pane_id("p_1").is_ok());
        assert!(validate_pane_id("").is_err());
        assert!(validate_pane_id("--help").is_err());
        assert!(validate_pane_id("p 1").is_err());
    }

    #[test]
    fn missing_binary_is_an_error() {
        let err = HerdrCli::new("/nonexistent/herdr").agents().unwrap_err();
        assert!(err.to_string().contains("could not run"));
    }
}
