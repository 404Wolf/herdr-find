//! End to end: spawn the binary, speak MCP over stdio, against a fake Herdr
//! (a shell script), a fake procfs and fixture transcripts.
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

fn fake_process(proc: &Path, pid: u32, pane: &str, open: Option<&Path>) {
    let dir = proc.join(pid.to_string());
    fs::create_dir_all(dir.join("fd")).unwrap();
    fs::write(
        dir.join("environ"),
        format!("TERM=xterm\0HERDR_PANE_ID={pane}\0"),
    )
    .unwrap();
    let mut stat = vec!["S".to_string(), "1".into()];
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

    let proc = root.join("proc");
    fake_process(&proc, 500, "p_parser", None);
    fake_process(&proc, 501, "p_thumbs", Some(&rollout));

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

    [
        ("HERDR_FIND_CLAUDE_DIR", claude),
        ("HERDR_FIND_CODEX_DIR", codex),
        ("HERDR_FIND_PROC_DIR", proc),
        ("HERDR_BIN_PATH", herdr),
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
    assert_eq!(found["sessions_searched"], 2);
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

    let listed = payload(&client.call("list_herdr_sessions", json!({})));
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|s| s["has_transcript"] == true));

    let focused = client.call("focus_herdr_pane", json!({"pane_id": "p_thumbs"}));
    assert_eq!(payload(&focused)["focused"], "p_thumbs");
    assert_eq!(
        fs::read_to_string(tmp.path().join("focused")).unwrap(),
        "p_thumbs\n"
    );

    let missing = client.call("focus_herdr_pane", json!({"pane_id": "p_gone"}));
    assert_eq!(missing["isError"], true, "{missing}");
    assert!(missing.to_string().contains("pane_not_found"));
}

#[test]
fn herdr_failure_is_a_tool_error() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    // Herdr isn't running: every command fails.
    fs::write(
        tmp.path().join("herdr"),
        "#!/bin/sh\necho '{\"error\":{\"code\":\"server_unavailable\"}}' >&2\nexit 1\n",
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
}

#[test]
fn cli_find_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fixture(tmp.path());
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_herdr-find"))
            .args(args)
            .env_remove("HERDR_PANE_ID")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let found = run(&["find", "--limit", "3", "nested", "tables"]);
    assert_eq!(found["matches"][0]["pane_id"], "p_parser");
    let listed = run(&["list"]);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    let focused = run(&["focus", "p_parser"]);
    assert_eq!(focused["focused"], "p_parser");
}
