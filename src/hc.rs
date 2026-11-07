//! herdr-core (`hc`) and the Herdr app (`herdr-app`).
//!
//! herdr-core runs the terminals; it knows each one's address (`t7`, or
//! `machine/t7` for another machine's), name, process, working directory,
//! OSC 7501 status and labels. It draws nothing: the Herdr app shows the
//! terminals in spaces and tabs and moves the user's view.
//!
//! Terminals are listed with one `terminal.list` call over `hc connect`
//! (JSON-RPC on stdin/stdout, so hc finds its own socket and session). Space
//! and tab names come from `herdr-app ls` when the app is open, else from the
//! `where` label the app leaves on its terminals. Focus is
//! `herdr-app do agent.focus --terminal <address>`.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::herdr::{AgentPane, CORE, Multiplexer, run_command, validate_pane_id};

/// Label naming the agent kind of a terminal (`claude`, `codex`).
pub const KIND_LABEL: &str = "agent.kind";
/// Label naming the agent's session id.
pub const SESSION_LABEL: &str = "agent.session";

/// herdr-core plus the Herdr app, driven through their CLIs.
#[derive(Debug, Clone)]
pub struct HerdrCore {
    hc_bin: String,
    app_bin: String,
    timeout: Duration,
}

/// One terminal as `terminal.list` reports it (the fields we use).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Terminal {
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub foreground: Option<Foreground>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Status {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Foreground {
    #[serde(default)]
    pub cwd: String,
}

#[derive(Deserialize)]
struct TerminalList {
    #[serde(default)]
    terminals: Vec<Terminal>,
}

/// `herdr-app ls`.
#[derive(Debug, Default, Deserialize)]
pub struct AppList {
    #[serde(default)]
    pub spaces: Vec<AppSpace>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AppSpace {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub tabs: Vec<AppTab>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AppTab {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub terminals: Vec<AppTerminal>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AppTerminal {
    pub terminal: String,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub sub_agents: Vec<Value>,
}

/// `herdr-app status` (the field we use).
#[derive(Debug, Default, Deserialize)]
struct AppStatus {
    #[serde(default)]
    focused_pane: Option<String>,
}

/// Parse a JSON-RPC response line from `hc connect`.
pub fn parse_terminal_list(out: &str) -> Result<Vec<Terminal>> {
    // One reply per line; take the first.
    let reply: Value = serde_json::Deserializer::from_str(out)
        .into_iter::<Value>()
        .next()
        .context("no reply from `hc connect`")?
        .context("unexpected output from `hc connect`")?;
    if let Some(error) = reply.get("error") {
        bail!("herdr-core: {error}");
    }
    let list: TerminalList = serde_json::from_value(reply["result"].clone())
        .context("unexpected terminal.list result")?;
    Ok(list.terminals)
}

/// A terminal title without leading spinner and status glyphs.
pub fn strip_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string()
}

/// A live terminal as an agent pane. `agent` is the `agent.kind` label, or
/// empty for the finder to detect; remote terminals (`machine/t7`) have no
/// local process, so they are kept only when labelled.
pub fn to_pane(t: Terminal, focused_terminal: Option<&str>) -> Option<AgentPane> {
    if t.state != "live" {
        return None;
    }
    let label = |k: &str| t.labels.get(k).filter(|v| !v.is_empty()).cloned();
    let agent = label(KIND_LABEL).unwrap_or_default();
    let remote = t.address.contains('/');
    if remote && agent.is_empty() {
        return None;
    }
    let status = t.status.as_ref();
    let title = Some(strip_title(&t.title))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            status
                .map(|s| strip_title(&s.title))
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    Some(AgentPane {
        focused: focused_terminal == Some(t.address.as_str()),
        status: status
            .map(|s| s.state.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        cwd: t
            .foreground
            .as_ref()
            .map(|f| f.cwd.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or(t.cwd.clone()),
        workspace_id: label("herdr.space").unwrap_or_default(),
        tab_id: label("herdr.tab").unwrap_or_default(),
        session_id: label(SESSION_LABEL),
        pid: if remote { None } else { t.pid },
        name: t.name.clone().unwrap_or_default(),
        runtime: CORE.into(),
        title,
        agent,
        pane_id: t.address,
    })
}

/// Space and tab names: the app's when it's open, else from the `where`
/// labels ("space / tab") on the terminals.
pub fn names(
    app: Option<&AppList>,
    terminals: &[Terminal],
) -> (HashMap<String, String>, HashMap<String, String>) {
    let (mut spaces, mut tabs) = (HashMap::new(), HashMap::new());
    for t in terminals {
        let (Some(space), Some(tab), Some(place)) = (
            t.labels.get("herdr.space"),
            t.labels.get("herdr.tab"),
            t.labels.get("where"),
        ) else {
            continue;
        };
        let (space_name, tab_name) = place.split_once(" / ").unwrap_or((place, ""));
        spaces.insert(space.clone(), space_name.trim().to_string());
        tabs.insert(tab.clone(), tab_name.trim().to_string());
    }
    for space in app.map(|a| a.spaces.as_slice()).unwrap_or_default() {
        let name = space.name.clone().filter(|n| !n.is_empty()).or_else(|| {
            space.folder.as_deref().map(|f| {
                f.rsplit('/')
                    .find(|p| !p.is_empty())
                    .unwrap_or(f)
                    .to_string()
            })
        });
        if let Some(name) = name {
            spaces.insert(space.id.clone(), name);
        }
        for (i, tab) in space.tabs.iter().enumerate() {
            let name = tab
                .name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| (i + 1).to_string());
            tabs.insert(tab.id.clone(), name);
        }
    }
    (spaces, tabs)
}

/// Whether `herdr-app ls` shows a terminal anywhere (in a pane, or as a
/// sub-agent under one).
pub fn shows(ls: &Value, address: &str) -> bool {
    match ls {
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| (k == "terminal" && v == address) || shows(v, address)),
        // Sub-agents may be listed by address alone.
        Value::Array(items) => items.iter().any(|v| v == address || shows(v, address)),
        _ => false,
    }
}

/// The terminal shown in the app's focused pane.
fn focused_terminal(app: &AppList, pane: &str) -> Option<String> {
    app.spaces
        .iter()
        .flat_map(|s| &s.tabs)
        .flat_map(|t| &t.terminals)
        .find(|t| t.pane.as_deref() == Some(pane))
        .map(|t| t.terminal.clone())
}

impl HerdrCore {
    pub fn new(hc_bin: impl Into<String>, app_bin: impl Into<String>) -> Self {
        Self {
            hc_bin: hc_bin.into(),
            app_bin: app_bin.into(),
            timeout: Duration::from_secs(15),
        }
    }

    /// Every live terminal herdr-core runs (this machine's and connected
    /// machines').
    pub fn terminals(&self) -> Result<Vec<Terminal>> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.list", "params": {}});
        let out = run_command(
            &self.hc_bin,
            &["connect"],
            Some(&format!("{request}\n")),
            self.timeout,
        )?;
        parse_terminal_list(&out)
    }

    fn app<T: serde::de::DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let out = run_command(&self.app_bin, args, None, self.timeout)?;
        serde_json::from_str(&out)
            .with_context(|| format!("unexpected output from `herdr-app {}`", args.join(" ")))
    }

    /// The app's layout, or `None` when no window is open.
    fn app_list(&self) -> Option<AppList> {
        self.app(&["ls"]).ok()
    }
}

impl Multiplexer for HerdrCore {
    fn agents(&self) -> Result<Vec<AgentPane>> {
        let terminals = self.terminals()?;
        let focused = self.app_list().and_then(|app| {
            let status: AppStatus = self.app(&["status"]).ok()?;
            focused_terminal(&app, &status.focused_pane?)
        });
        Ok(terminals
            .into_iter()
            .filter_map(|t| to_pane(t, focused.as_deref()))
            .collect())
    }

    fn workspaces(&self) -> Result<HashMap<String, String>> {
        Ok(names(self.app_list().as_ref(), &self.terminals()?).0)
    }

    fn tabs(&self) -> Result<HashMap<String, String>> {
        Ok(names(self.app_list().as_ref(), &self.terminals()?).1)
    }

    fn focus(&self, pane_id: &str) -> Result<()> {
        validate_pane_id(pane_id)?;
        // Accept a terminal's name too; the app wants its address.
        let address = self
            .terminals()?
            .into_iter()
            .find(|t| {
                t.state == "live" && (t.address == pane_id || t.name.as_deref() == Some(pane_id))
            })
            .map(|t| t.address)
            .with_context(|| format!("herdr-core has no live terminal {pane_id:?}"))?;
        // A terminal with no pane in the app (e.g. started by `hc run` and
        // never given one) can't be focused; peek at it over the tab instead.
        let shown = self.app::<Value>(&["ls"]).map(|ls| shows(&ls, &address));
        let command = if matches!(shown, Ok(false)) {
            "terminal.peek"
        } else {
            "agent.focus"
        };
        let result: Value = self.app(&["do", command, "--terminal", &address]).with_context(|| {
            format!("the Herdr app could not show {address} (is it open? `hc attach {address}` works without it)")
        })?;
        if let Some(error) = result.get("error") {
            bail!("the Herdr app could not show {address}: {error}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"{"id":1,"jsonrpc":"2.0","result":{"hidden_finished":1,"terminals":[
        {"address":"t3","argv":["bash"],"cols":200,"created_ms":1,"cwd":"/tmp","foreground":{"argv":["claude"],"cwd":"/work/parser","name":"claude","pid":42},
         "labels":{"herdr.space":"sp1","herdr.tab":"tb1","where":"parsers / main","agent.kind":"claude","agent.session":"sess-1"},
         "name":"parser","pid":40,"rows":50,"screen":"exact","state":"live","status":{"state":"working","title":"x"},"title":"✳ Refactor parser","turn":2},
        {"address":"t4","argv":["bash"],"cwd":"/tmp","labels":{},"name":null,"pid":50,"state":"live","status":null,"title":"","turn":0},
        {"address":"t5","cwd":"/tmp","labels":{"agent.kind":"codex"},"pid":null,"state":"finished"},
        {"address":"vps/t1","cwd":"/srv","labels":{},"pid":7,"state":"live"},
        {"address":"vps/t2","cwd":"/srv","labels":{"agent.kind":"codex","herdr.space":"sp2","herdr.tab":"tb2"},"pid":8,"state":"live","status":{"state":"idle","title":"⠂ Deploy fix"}}
    ],"machines":[]}}"#;

    #[test]
    fn parses_terminal_list() {
        let terminals = parse_terminal_list(LIST).unwrap();
        assert_eq!(terminals.len(), 5);
        assert_eq!(terminals[0].pid, Some(40));
        assert_eq!(terminals[0].labels["agent.session"], "sess-1");
        assert_eq!(terminals[1].name, None);
        assert!(
            parse_terminal_list(r#"{"id":1,"error":{"code":-32000,"message":"boom"}}"#)
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        assert!(parse_terminal_list("").is_err());
        assert!(parse_terminal_list("not json").is_err());
    }

    #[test]
    fn maps_terminals_to_panes() {
        let panes: Vec<AgentPane> = parse_terminal_list(LIST)
            .unwrap()
            .into_iter()
            .filter_map(|t| to_pane(t, Some("t4")))
            .collect();
        // t5 is finished; vps/t1 is remote and unlabelled.
        let ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, ["t3", "t4", "vps/t2"]);
        assert_eq!(
            panes[0],
            AgentPane {
                pane_id: "t3".into(),
                agent: "claude".into(),
                status: "working".into(),
                cwd: "/work/parser".into(),
                workspace_id: "sp1".into(),
                tab_id: "tb1".into(),
                title: "Refactor parser".into(),
                focused: false,
                runtime: CORE.into(),
                name: "parser".into(),
                pid: Some(40),
                session_id: Some("sess-1".into()),
            }
        );
        assert_eq!(panes[1].agent, "");
        assert_eq!(panes[1].status, "unknown");
        assert!(panes[1].focused);
        assert_eq!(panes[1].pid, Some(50));
        assert_eq!(panes[2].pid, None);
        assert_eq!(panes[2].title, "Deploy fix");
    }

    #[test]
    fn names_spaces_and_tabs() {
        let terminals = parse_terminal_list(LIST).unwrap();
        let (spaces, tabs) = names(None, &terminals);
        assert_eq!(spaces["sp1"], "parsers");
        assert_eq!(tabs["tb1"], "main");
        assert!(!spaces.contains_key("sp2"));

        let app: AppList = serde_json::from_str(
            r#"{"spaces":[{"id":"sp1","name":"Parsing","folder":"/w","active":true,"tabs":[
                {"id":"tb1","name":null,"terminals":[{"terminal":"t3","pane":"p1","sub_agents":[]}]},
                {"id":"tb9","name":"logs","terminals":[]}]},
               {"id":"sp2","name":null,"folder":"/srv/app/","tabs":[]}],"recent":[]}"#,
        )
        .unwrap();
        let (spaces, tabs) = names(Some(&app), &terminals);
        assert_eq!(spaces["sp1"], "Parsing");
        assert_eq!(spaces["sp2"], "app");
        assert_eq!(tabs["tb1"], "1");
        assert_eq!(tabs["tb9"], "logs");
        assert_eq!(focused_terminal(&app, "p1").as_deref(), Some("t3"));
        assert_eq!(focused_terminal(&app, "p2"), None);
    }

    #[test]
    fn knows_which_terminals_the_app_shows() {
        let ls: Value = serde_json::from_str(
            r#"{"spaces":[{"id":"s","tabs":[{"id":"t","terminals":[
                {"terminal":"t3","pane":"p1","sub_agents":[{"terminal":"t8","name":"helper"},"t9"]}]}]}],"recent":[]}"#,
        )
        .unwrap();
        assert!(shows(&ls, "t3"));
        assert!(shows(&ls, "t8"));
        assert!(shows(&ls, "t9"));
        assert!(!shows(&ls, "t1y"));
        assert!(!shows(&ls, "p1"));
    }

    #[test]
    fn strips_title_glyphs() {
        assert_eq!(strip_title("✳ Fix the parser"), "Fix the parser");
        assert_eq!(strip_title("⠐ codex"), "codex");
        assert_eq!(strip_title(""), "");
    }

    #[test]
    fn missing_binaries_are_errors() {
        let core = HerdrCore::new("/nonexistent/hc", "/nonexistent/herdr-app");
        assert!(
            core.agents()
                .unwrap_err()
                .to_string()
                .contains("could not run")
        );
        assert!(core.focus("t1").is_err());
        assert!(core.focus("--help").is_err());
    }
}
