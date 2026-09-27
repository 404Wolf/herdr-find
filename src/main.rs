use std::process::ExitCode;

use herdr_find::config::Config;
use herdr_find::finder::Finder;
use herdr_find::mcp::{self, DEFAULT_LIMIT};

const USAGE: &str = "\
herdr-find: find a running Herdr agent session by what it's about.

Usage:
  herdr-find [mcp]                      Serve MCP on stdio (default)
  herdr-find find [--limit N] [--all] <description...>
                                        Rank running sessions against a description
  herdr-find list                       List running sessions
  herdr-find focus <pane_id>            Switch the Herdr view to a pane
  herdr-find --help | --version

`find` leaves out the pane herdr-find runs in (HERDR_PANE_ID) unless --all.
Output is JSON.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("herdr-find: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let finder = || Finder::from_config(&Config::from_env());
    let print = |value: serde_json::Value| {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("serializable")
        );
    };
    match args.first().map(String::as_str) {
        None | Some("mcp") | Some("serve") => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(mcp::serve(finder()))
        }
        Some("find") => {
            let (mut limit, mut all, mut words) = (DEFAULT_LIMIT, false, Vec::new());
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--limit" | "-n" => {
                        limit = rest
                            .next()
                            .and_then(|n| n.parse().ok())
                            .ok_or_else(|| anyhow::anyhow!("--limit needs a number"))?;
                    }
                    "--all" => all = true,
                    _ => words.push(arg.as_str()),
                }
            }
            if words.is_empty() {
                anyhow::bail!("find needs a description\n\n{USAGE}");
            }
            let finder = finder();
            let (matches, searched) = finder.find(&words.join(" "), limit.max(1), all)?;
            print(serde_json::json!({
                "matches": matches,
                "sessions_searched": searched,
                "warnings": finder.warnings(),
            }));
            Ok(())
        }
        Some("list") => {
            let finder = finder();
            print(serde_json::json!({
                "sessions": finder.list()?,
                "warnings": finder.warnings(),
            }));
            Ok(())
        }
        Some("focus") => {
            let [_, pane] = args else {
                anyhow::bail!("usage: herdr-find focus <pane_id>");
            };
            finder().focus(pane)?;
            print(serde_json::json!({ "focused": pane }));
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("herdr-find {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}
