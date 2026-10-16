//! space-kit CLI: the commands Herdr's manifest and Claude Code's statusline call.
use space_kit::herdr::client::{Socket, socket_path};
use space_kit::model::{PLUGIN_ID, state_dir};
use space_kit::native::sync;
use space_kit::{tui, usage};
use std::io::Read;
use std::process::{Command, ExitCode, Stdio};

const USAGE: &str = "usage: space-kit <command>

  sync [--quiet] [--force] [--on-focus]   order the sidebar and report its tokens
  toggle-collapse [--workspace ID]         fold or unfold a workspace's group
  open                                     open the group manager popup
  manage                                   run the group manager in this terminal
  record                                   cache Claude's statusline rate limits (JSON on stdin)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("space-kit: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    let has = |flag: &str| rest.iter().any(|a| a == flag);
    let dir = state_dir();
    match command.as_str() {
        "sync" => {
            if let Some(bad) = rest.iter().find(|a| !["--quiet", "--force", "--on-focus"].contains(&a.as_str())) {
                return Err(format!("unknown argument {bad}\n{USAGE}"));
            }
            if has("--on-focus") && !sync::focus_needs_sync(&dir) {
                return Ok(());
            }
            let summary = sync::sync(&Socket, &dir, has("--force"), false)?;
            if !has("--quiet") {
                println!("{summary}");
            }
            Ok(())
        }
        "toggle-collapse" => {
            let workspace = match rest {
                [] => sync::context_workspace_id(),
                [flag, id] if flag == "--workspace" => Some(id.clone()),
                _ => return Err(USAGE.into()),
            };
            println!("{}", sync::toggle_collapse(&Socket, &dir, workspace.as_deref())?);
            Ok(())
        }
        "open" => open_manager(),
        "manage" => tui::run(dir),
        "record" => record(),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

fn open_manager() -> Result<(), String> {
    let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let plugin = std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| PLUGIN_ID.into());
    let status = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", &plugin, "--entrypoint", "manager"])
        .status()
        .map_err(|e| format!("herdr: {e}"))?;
    status.success().then_some(()).ok_or_else(|| format!("Herdr could not open the group manager ({status})"))
}

/// Called by Claude Code's statusline script: cache changed rate limits, then sync in the
/// background so the Agents panel title follows Claude in real time.
fn record() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
    let changed = usage::record(&input, &usage::claude_cache()).map_err(|e| e.to_string())?;
    if changed && socket_path().exists() {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Command::new(exe)
            .args(["sync", "--quiet"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
