# herdr-find

An [MCP](https://modelcontextprotocol.io) server that takes you to a running
[Herdr](https://herdr.dev) agent session by describing it.

> "Take me to the herdr window where we're refactoring the config parser."

Any coding agent with this server attached can answer that. `herdr-find`
lists every agent session Herdr knows about (Claude Code, Codex, ...), finds
the transcript behind each one, ranks them against the description, and
focuses the chosen one.

It works with both Herdr runtimes, side by side:

- the **Herdr TUI** (`herdr`): agent panes from `herdr agent list`, focused
  with `herdr agent focus`;
- **herdr-core** (`hc`) with the **Herdr app**: terminals from `hc`'s
  `terminal.list` (over `hc connect`), shown with
  `herdr-app do agent.focus --terminal t7` (or `terminal.peek` for a terminal
  the app has no pane for).

Each session says which `runtime` it came from (`herdr` or `herdr-core`). For
herdr-core, `pane_id` is the terminal address (`t7`); `focus` also takes the
terminal's name. If one runtime isn't running, the other's sessions are still
returned, with a warning.

## Tools

| Tool | Arguments | What it does |
|---|---|---|
| `find_herdr_session` | `description` (string, required), `limit` (int, default 5), `include_current` (bool, default false) | Ranks running sessions against the description. Returns `matches` (best first) and `sessions_searched`. |
| `list_herdr_sessions` | none | Every running session with the same summary fields, so the agent can judge when keywords aren't enough. |
| `focus_herdr_pane` | `pane_id` (string, required) | Switches the user's Herdr view to that pane, including its workspace and tab. |

Each session is summarised as `pane_id`, `agent`, `status`, `title`,
`summary` (the agent's own conversation title, when it writes one),
`workspace`, `tab`, `cwd`, `focused`, `is_current_pane`, `has_transcript`,
`idle_seconds`, `first_prompt` and `recent_prompts`. Matches add `score` and
the best-matching `snippet`. Prompts and snippets are cut short. Whole
transcripts are never returned.

The calling agent's own pane or terminal (its `HERDR_PANE_ID` or
`HC_TERMINAL_ID`) is left out of `find_herdr_session`, because its transcript
contains the request itself.

Results come back as structured content plus the same JSON as text. When no
Herdr runtime is reachable, the tools return errors instead of crashing.

## How panes map to transcripts

Herdr sets `HERDR_PANE_ID` in the environment of everything it starts in a
pane, so every agent process carries its pane id in `/proc/<pid>/environ`.

- **Claude Code** writes `~/.claude/sessions/<pid>.json` with its `sessionId`
  (and `procStart`, which is checked against `/proc/<pid>/stat` so a reused
  pid isn't mistaken for the agent). The transcript is
  `~/.claude/projects/*/<sessionId>.jsonl`.
- **Codex** keeps its current rollout open, so the transcript is the
  `~/.codex/sessions/**/rollout-*.jsonl` in `/proc/<pid>/fd`. If none is open,
  the thread id comes from the `~/.codex/thread-writer-locks/<id>.lock` it
  holds.

If an agent was started from inside another agent's pane, the outermost
process wins.

**herdr-core terminals** are matched by process tree instead: the agent is
the outermost Claude Code / Codex process under the terminal's `pid`
(recognised by its command line, a sessions file or an open rollout), and its
transcript is found as above, falling back to Claude Code's
`--resume <id>` / `--session-id <id>` argument. Labels win when present:
`agent.kind=claude|codex` names the agent and `agent.session=<id>` its
session (Codex: the thread id at the end of the rollout's name). The walk
doesn't descend into processes of a Herdr TUI pane running inside the
terminal. Codex rollouts of sub-agent threads (first-line `session_meta`
with a `{"subagent": ...}` source) are never taken for the session. Space and
tab names come from `herdr-app ls`, else from the `where` label the app sets.
Terminals on other machines (`vps/t7`) are listed only when labelled. Transcripts can be huge, so only the first 256 KiB (for the
opening prompt) and the last 4 MiB (recent context) are read. Only human
prompts, assistant prose and conversation titles are used. Tool calls, tool
output, thinking and injected harness text (`<command-name>`,
`<environment_context>`, `# AGENTS.md`, ...) are skipped. A session whose
transcript can't be found (e.g. one with no messages yet) is still listed and
searched by its title, workspace, tab and directory.

Mapping needs Linux `/proc`. On macOS the server builds and runs, but it
searches only the metadata Herdr reports and adds a warning saying so.

## Ranking

The default `KeywordRanker` tokenises the description, drops stopwords
("take me to the herdr window where we ...") and lightly stems words
(`enums` ~ `enum`, `caching` ~ `cache`, `exhaustive` ~ `exhaustively`). Each
query word scores the weights of the fields it appears in:

| Field | Weight |
|---|---|
| Pane title and conversation summaries | 4 |
| Workspace, tab, working directory, agent kind | 2 |
| The human's prompts | 2 |
| The last 40 assistant replies | 1 |

Words are weighted by inverse document frequency across the running sessions,
and the total is scaled by how much of the query a session covers. A session
that mentions all of the query in its prompts beats one that repeats a single
word everywhere.

## Privacy

`herdr-find` reads local transcript files and talks to the local `herdr` CLI.
It makes no network requests, and it never writes or sends transcripts
anywhere. Search results (short titles, prompts and snippets) go only to the
agent that called the tool, over stdio.

## Install

As a flake input:

```nix
{
  inputs.herdr-find = {
    url = "github:404wolf/herdr-find";
    inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

Then register it with the agents Home Manager configures through
`programs.mcp`:

```nix
{ inputs, lib, pkgs, ... }:
{
  programs.mcp = {
    enable = true;
    servers.herdr = {
      command = lib.getExe inputs.herdr-find.packages.${pkgs.stdenv.hostPlatform.system}.default;
      args = [ "mcp" ];
    };
  };
}
```

Or for Claude Code directly:

```sh
claude mcp add --scope user herdr -- herdr-find mcp
```

Flake outputs: `packages.<system>.{default,herdr-find}`, `overlays.default`
(adds `pkgs.herdr-find`), `devShells.<system>.default`, `formatter.<system>`
and `checks.<system>.herdr-find`, for `x86_64-linux`, `aarch64-linux`,
`x86_64-darwin` and `aarch64-darwin`.

With Cargo: `cargo install --git https://github.com/404wolf/herdr-find`.

## CLI

```sh
herdr-find                        # serve MCP on stdio (same as `herdr-find mcp`)
herdr-find find "config parser"   # rank sessions; --limit N, --all to include this pane
herdr-find list                   # list sessions
herdr-find focus <pane_id>        # focus a TUI pane, or a herdr-core terminal (t7 or its name)
```

Output is JSON.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `HERDR_BIN_PATH` | `herdr` on `PATH` | Herdr binary. Herdr exports this itself; a trailing ` (deleted)` is stripped. |
| `HERDR_PANE_ID` | none | The caller's pane, which `find` leaves out. Herdr sets this. |
| `HC_BIN_PATH` | `hc` on `PATH` | herdr-core CLI. |
| `HERDR_APP_BIN_PATH` | `herdr-app` on `PATH` | Herdr app CLI. |
| `HC_TERMINAL_ID` | none | The caller's herdr-core terminal, which `find` leaves out. herdr-core sets this. |
| `HERDR_FIND_RUNTIMES` | `herdr,core` | Which runtimes to search: `herdr` (TUI), `core` (herdr-core), or both. |
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Claude Code config dir. |
| `CODEX_HOME` | `~/.codex` | Codex home. |
| `HERDR_FIND_CLAUDE_DIR` | none | Overrides `CLAUDE_CONFIG_DIR` for `herdr-find` only. |
| `HERDR_FIND_CODEX_DIR` | none | Overrides `CODEX_HOME` for `herdr-find` only. |
| `HERDR_FIND_PROC_DIR` | `/proc` | procfs root, for tests. |

## Adding an agent

Each agent is one module in `src/sources/` implementing `AgentSource`:

```rust
pub trait AgentSource: Send + Sync {
    /// The agent name Herdr reports (`herdr agent list` → `agent`).
    fn kind(&self) -> &str;
    /// Live processes of this agent → (pid, pane id, transcript path).
    fn locate(&self, procs: &dyn ProcessTable) -> Vec<Candidate>;
    /// Whether a command line is this agent's process (herdr-core).
    fn is_agent(&self, argv: &[String]) -> bool;
    /// The transcript one live process writes (herdr-core).
    fn transcript_of_process(&self, pid: u32, procs: &dyn ProcessTable) -> Option<PathBuf>;
    /// The transcript of a session id from an `agent.session` label.
    fn transcript_of_session(&self, session_id: &str) -> Option<PathBuf>;
    /// Fold one JSONL line into the transcript (titles, prompts, replies).
    fn parse_line(&self, line: &str, out: &mut Transcript);
}
```

Register it with one line in `default_sources` in `src/sources/mod.rs`.
`ProcessTable` gives you `pane_of(pid)`, `open_files(pid)`, `cmdline(pid)`,
`env_var` and `start_time`. `Transcript::push_user` already filters out injected harness
text.

The other seams are traits as well. `Multiplexer` covers listing panes,
workspaces and tabs and focusing a pane (`HerdrCli` for the TUI, `HerdrCore`
for herdr-core and the app, `Runtimes` for several at once; the tests use a
mock). `Ranker` covers ordering sessions (`KeywordRanker`).

## Development

```sh
nix develop
cargo test
cargo clippy --all-targets -- -D warnings
nix fmt
nix build
```

The tests use small synthetic transcripts in `tests/fixtures/`, a fake procfs
and a mock `herdr` script. `tests/mcp_stdio.rs` spawns the binary and runs a
full MCP session over stdio.

## License

MIT
