//! End to end: spawn the binary, speak MCP over stdio, against a fake Herdr
//! TUI, a fake herdr-core and Herdr app (shell scripts), a fake procfs and
//! fixture transcripts.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const AGENTS: &str = r#"{"id":"1","result":{"type":"agent_list","agents":[
{"pane_id":"p_parser","agent":"claude","agent_status":"idle","cwd":"/work/demo","workspace_id":"w_1","tab_id":"t_1","terminal_title_stripped":"Claude Code","focused":false},
{"pane_id":"p_thumbs","agent":"codex","agent_status":"working","cwd":"/work/images","workspace_id":"w_2","tab_id":"t_2","terminal_title_stripped":"codex","focused":true}]}}"#;
const WORKSPACES: &str = r#"{"id":"2","result":{"workspaces":[{"workspace_id":"w_1","label":"demo"},{"workspace_id":"w_2","label":"images"}]}}"#;
const TABS: &str = r#"{"id":"3","result":{"tabs":[{"tab_id":"t_1","workspace_id":"w_1","label":"main"},{"tab_id":"t_2","workspace_id":"w_2","label":"main"}]}}"#;

/// herdr-core's `terminal.list` reply: a shell terminal (pid 600) running
/// Claude Code (pid 601), unlabelled, shown in the app; and a plain shell.
const HC_LIST: &str = r#"{"id":1,"jsonrpc":"2.0","result":{"hidden_finished":0,"machines":[],"terminals":[
{"address":"t9","argv":["zsh"],"cwd":"/work/ops","labels":{"herdr.space":"sp_ops","herdr.tab":"tb_deploy","where":"ops / deploy"},"name":"deployer","pid":600,"state":"live","status":{"state":"idle","title":"Claude Code"},"title":"✳ Claude Code","turn":1},
{"address":"t10","argv":["zsh"],"cwd":"/work/ops","labels":{},"name":null,"pid":610,"state":"live","status":null,"title":"zsh","turn":0}]}}"#;
const APP_LS: &str = r#"{"spaces":[{"id":"sp_ops","name":"ops","folder":"/work/ops","tabs":[{"id":"tb_deploy","name":"deploy","terminals":[{"terminal":"t9","pane":"pn_1","sub_agents":[]}]}]}],"recent":[]}"#;
const APP_STATUS: &str = r#"{"space":"sp_ops","tab":"tb_deploy","focused_pane":"pn_9"}"#;

fn fake_process(proc: &Path, pid: u32, pane: &str, open: Option<&Path>) {
    fake_child(proc, pid, 1, &format!("HERDR_PANE_ID={pane}"), &[], open);
}

fn fake_child(proc: &Path, pid: u32, ppid: u32, env: &str, argv: &[&str], open: Option<&Path>) {
    let dir = proc.join(pid.to_string());
    fs::create_dir_all(dir.join("fd")).unwrap();
    fs::write(dir.join("environ"), format!("TERM=xterm\0{env}\0")).unwrap();
    let cmdline: String = argv.iter().map(|a| format!("{a}\0")).collect();
    fs::write(dir.join("cmdline"), cmdline).unwrap();
    let mut stat = vec!["S".to_string(), ppid.to_string()];
    stat.extend((5..22).map(|_| "0".to_string()));
    stat.push("777".into());
    fs::write(
        dir.join("stat"),
        format!("{pid} (agent) {}\n", stat.join(" ")),
    )
    .unwrap();
    if let Some(target) = open {
        symlink(target, dir.join("fd/3")).unwrap();
    }
}

/// Lay out fixtures and a mock herdr; return the environment to run with.
fn fixture(root: &Path) -> Vec<(String, String)> {
    let claude = root.join("claude");
    fs::create_dir_all(claude.join("sessions")).unwrap();
    fs::create_dir_all(claude.join("projects/-work-demo")).unwrap();
    fs::write(
        claude.join("projects/-work-demo/sess-1.jsonl"),
        include_str!("fixtures/claude.jsonl"),
    )
    .unwrap();
    fs::write(
        claude.join("sessions/500.json"),
        r#"{"pid":500,"sessionId":"sess-1","procStart":"777"}"#,
    )
    .unwrap();

    let codex = root.join("codex");
    let day = codex.join("sessions/2026/01/01");
    fs::create_dir_all(&day).unwrap();
    let rollout = day.join("rollout-2026-01-01T00-00-00-y.jsonl");
    fs::write(&rollout, include_str!("fixtures/codex.jsonl")).unwrap();

    fs::write(
        claude.join("projects/-work-demo/sess-2.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Port the deploy script to nushell"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rewriting deploy.sh as deploy.nu."}]}}"#,
    )
    .unwrap();

    let proc = root.join("proc");
    fake_process(&proc, 500, "p_parser", None);
    fake_process(&proc, 501, "p_thumbs", Some(&rollout));
    // herdr-core terminal t9: zsh (600) → claude --resume sess-2 (601).
    fake_child(&proc, 600, 1, "HC_TERMINAL_ID=t9", &["zsh"], None);
    fake_child(
        &proc,
        601,
        600,
        "HC_TERMINAL_ID=t9",
        &["/nix/store/x/bin/claude", "--resume", "sess-2"],
        None,
    );
    fake_child(&proc, 610, 1, "HC_TERMINAL_ID=t10", &["zsh"], None);

    for (name, body) in [
        ("agents.json", AGENTS),
        ("workspaces.json", WORKSPACES),
        ("tabs.json", TABS),
    ] {
        fs::write(root.join(name), body).unwrap();
    }
    let herdr = root.join("herdr");
    fs::write(
        &herdr,
        format!(
            r#"#!/bin/sh
dir='{dir}'
case "$1 $2" in
  "agent list") cat "$dir/agents.json" ;;
  "workspace list") cat "$dir/workspaces.json" ;;
  "tab list") cat "$dir/tabs.json" ;;
  "agent focus")
    case "$3" in
      p_parser|p_thumbs) echo "$3" >> "$dir/focused"; echo '{{"id":"4","result":{{}}}}' ;;
      *) echo '{{"error":{{"code":"pane_not_found"}}}}' >&2; exit 1 ;;
    esac ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            dir = root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();

    for (name, body) in [
        ("hc.json", HC_LIST),
        ("app-ls.json", APP_LS),
        ("app-status.json", APP_STATUS),
    ] {
        fs::write(root.join(name), body).unwrap();
    }
    let hc = root.join("hc");
    fs::write(
        &hc,
        format!(
            r#"#!/bin/sh
dir='{dir}'
[ "$1" = connect ] || {{ echo "unexpected: $*" >&2; exit 2; }}
grep -q terminal.list || exit 3
cat "$dir/hc.json" | tr -d '\n'; echo
"#,
            dir = root.display()
        ),
    )
    .unwrap();
    let app = root.join("herdr-app");
    fs::write(
        &app,
        format!(
            r#"#!/bin/sh
dir='{dir}'
case "$1" in
  ls) cat "$dir/app-ls.json" ;;
  status) cat "$dir/app-status.json" ;;
  do)
    case "$2 $3" in
      "agent.focus --terminal"|"terminal.peek --terminal") ;;
      *) echo '{{"error":"bad command"}}'; exit 1 ;;
    esac
    echo "$2:$4" >> "$dir/focused"; echo '{{"ok":true}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            dir = root.display()
        ),
    )
    .unwrap();
    for f in [&hc, &app] {
        fs::set_permissions(f, fs::Permissions::from_mode(0o755)).unwrap();
    }

    [
        ("HERDR_FIND_CLAUDE_DIR", claude),
        ("HERDR_FIND_CODEX_DIR", codex),
        ("HERDR_FIND_PROC_DIR", proc),
        ("HERDR_BIN_PATH", herdr),
        ("HC_BIN_PATH", hc),
        ("HERDR_APP_BIN_PATH", app),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.display().to_string()))
    .collect()
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn spawn(env: &[(String, String)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-find"))
            .arg("mcp")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HERDR_FIND_RUNTIMES")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert_ne!(
                self.stdout.read_line(&mut line).unwrap(),
                0,
                "server hung up"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        response["result"].clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The JSON payload of a tool result's text content.
fn payload(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn mcp_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    let mut client = Client::spawn(&env);

    let init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "herdr-find");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("focus_herdr_pane")
    );
    client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let tools = client.request("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "find_herdr_session",
            "focus_herdr_pane",
            "list_herdr_sessions"
        ]
    );
    let find = tools
        .iter()
        .find(|t| t["name"] == "find_herdr_session")
        .unwrap();
    assert_eq!(find["inputSchema"]["required"], json!(["description"]));
    assert!(
        find["description"]
            .as_str()
            .unwrap()
            .contains("take them to")
    );

    let found = client.call(
        "find_herdr_session",
        json!({"description": "the herdr window where we refactor the config parser"}),
    );
    assert_ne!(found["isError"], json!(true), "{found}");
    let found = payload(&found);
    assert_eq!(found["sessions_searched"], 3);
    let top = &found["matches"][0];
    assert_eq!(top["pane_id"], "p_parser");
    assert_eq!(top["workspace"], "demo");
    assert_eq!(top["tab"], "main");
    assert_eq!(top["summary"], "Refactor config parser for nested tables");
    assert!(top["snippet"].as_str().unwrap().contains("parser"));
    assert!(top["score"].as_f64().unwrap() > 0.0);

    let found = payload(&client.call(
        "find_herdr_session",
        json!({"description": "thumbnail cache benchmark", "limit": 1}),
    ));
    assert_eq!(found["matches"].as_array().unwrap().len(), 1);
    assert_eq!(found["matches"][0]["pane_id"], "p_thumbs");
    assert_eq!(found["matches"][0]["agent"], "codex");

    // herdr-core: the agent found under the terminal's shell.
    let found = payload(&client.call(
        "find_herdr_session",
        json!({"description": "take me to where we port the deploy script to nushell"}),
    ));
    let top = &found["matches"][0];
    assert_eq!(top["pane_id"], "t9", "{found}");
    assert_eq!(top["runtime"], "herdr-core");
    assert_eq!(top["agent"], "claude");
    assert_eq!(top["name"], "deployer");
    assert_eq!(top["workspace"], "ops");
    assert_eq!(top["tab"], "deploy");
    assert_eq!(top["title"], "Claude Code");
    assert_eq!(top["first_prompt"], "Port the deploy script to nushell");

    let listed = payload(&client.call("list_herdr_sessions", json!({})));
    let sessions = listed["sessions"].as_array().unwrap();
    // t10 runs no agent.
    assert_eq!(sessions.len(), 3, "{listed}");
    assert!(sessions.iter().all(|s| s["has_transcript"] == true));
    assert!(listed.get("warnings").is_none(), "{listed}");

    let focused = client.call("focus_herdr_pane", json!({"pane_id": "p_thumbs"}));
    assert_eq!(payload(&focused)["focused"], "p_thumbs");
    assert_eq!(
        fs::read_to_string(tmp.path().join("focused")).unwrap(),
        "p_thumbs\n"
    );
    // By address or by name, through the Herdr app; a terminal with no pane
    // in the app is peeked at.
    client.call("focus_herdr_pane", json!({"pane_id": "t9"}));
    let by_name = client.call("focus_herdr_pane", json!({"pane_id": "deployer"}));
    assert_ne!(by_name["isError"], json!(true), "{by_name}");
    let paneless = client.call("focus_herdr_pane", json!({"pane_id": "t10"}));
    assert_ne!(paneless["isError"], json!(true), "{paneless}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("focused")).unwrap(),
        "p_thumbs\nagent.focus:t9\nagent.focus:t9\nterminal.peek:t10\n"
    );

    let missing = client.call("focus_herdr_pane", json!({"pane_id": "p_gone"}));
    assert_eq!(missing["isError"], true, "{missing}");
    assert!(missing.to_string().contains("pane_not_found"));
}

#[test]
fn herdr_failure_is_a_tool_error() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    // Neither runtime is running: every command fails.
    fs::write(
        tmp.path().join("herdr"),
        "#!/bin/sh\necho '{\"error\":{\"code\":\"server_unavailable\"}}' >&2\nexit 1\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("hc"),
        "#!/bin/sh\necho 'hc: no core running' >&2\nexit 1\n",
    )
    .unwrap();
    let mut client = Client::spawn(&env);
    client.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let result = client.call("list_herdr_sessions", json!({}));
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        result.to_string().contains("server_unavailable"),
        "{result}"
    );
    assert!(result.to_string().contains("no core running"), "{result}");
}

#[test]
fn one_runtime_down_is_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    // The Herdr TUI is gone; herdr-core still answers.
    fs::write(tmp.path().join("herdr"), "#!/bin/sh\nexit 1\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-find"))
        .arg("list")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HERDR_FIND_RUNTIMES")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["pane_id"], "t9");
    assert!(
        listed["warnings"][0]
            .as_str()
            .unwrap()
            .contains("herdr not reachable")
    );

    // Only herdr-core, by choice: no warning about the TUI.
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-find"))
        .arg("list")
        .env_remove("HERDR_PANE_ID")
        .env("HC_TERMINAL_ID", "t9")
        .env("HERDR_FIND_RUNTIMES", "core")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        listed["warnings"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    assert_eq!(listed["sessions"][0]["is_current_pane"], true);
}

#[test]
fn cli_find_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr-find"))
            .args(args)
            .env_remove("HERDR_PANE_ID")
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HERDR_FIND_RUNTIMES")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let found = run(&["find", "--limit", "3", "nested", "tables"]);
    assert_eq!(found["matches"][0]["pane_id"], "p_parser");
    let listed = run(&["list"]);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 3);
    let focused = run(&["focus", "p_parser"]);
    assert_eq!(focused["focused"], "p_parser");
}
