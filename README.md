# herdr-find

An [MCP](https://modelcontextprotocol.io) server that takes you to a running
[Herdr](https://herdr.dev) agent session by describing it.

> "Take me to the herdr window where we're refactoring the config parser."

Any coding agent with this server attached can answer that. `herdr-find`
lists every agent pane Herdr knows about (Claude Code, Codex, ...), finds the
transcript behind each one, ranks them against the description, and focuses
the chosen pane with `herdr agent focus`.

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

The calling agent's own pane (its `HERDR_PANE_ID`) is left out of
`find_herdr_session`, because its transcript contains the request itself.

Results come back as structured content plus the same JSON as text. When Herdr
isn't reachable, the tools return errors instead of crashing.

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
process wins. Transcripts can be huge, so only the first 256 KiB (for the
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
herdr-find focus <pane_id>        # focus a pane
```

Output is JSON.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `HERDR_BIN_PATH` | `herdr` on `PATH` | Herdr binary. Herdr exports this itself; a trailing ` (deleted)` is stripped. |
| `HERDR_PANE_ID` | none | The caller's pane, which `find` leaves out. Herdr sets this. |
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
    /// Fold one JSONL line into the transcript (titles, prompts, replies).
    fn parse_line(&self, line: &str, out: &mut Transcript);
}
```

Register it with one line in `default_sources` in `src/sources/mod.rs`.
`ProcessTable` gives you `pane_of(pid)`, `open_files(pid)`, `env_var` and
`start_time`. `Transcript::push_user` already filters out injected harness
text.

The other seams are traits as well. `Multiplexer` covers listing panes,
workspaces and tabs and focusing a pane (`HerdrCli` implements it, and the
tests use a mock). `Ranker` covers ordering sessions (`KeywordRanker`).

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
