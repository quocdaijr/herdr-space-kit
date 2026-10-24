//! The `space-kit` binary end to end, against a fake Herdr socket, a temp state dir and a temp
//! HOME, so no test touches a live Herdr or the real ~/.cache and ~/.local/state.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    dir: PathBuf,
    socket: PathBuf,
}

/// A fake Herdr answering every request; it serves until the test process exits.
fn fake_herdr(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "session.snapshot" => json!({"snapshot": {
                            "focused_workspace_id": "p1",
                            "workspaces": [
                                {"workspace_id": "p1", "label": "api", "worktree": {"repo_root": "/space-kit-cli/api", "repo_key": "/space-kit-cli/api/.git"}},
                                {"workspace_id": "h1", "label": "⟦Work⟧"},
                            ],
                            "agents": [], "panes": [],
                        }}),
                        _ => json!({}),
                    };
                    let reply = json!({"id": request["id"], "result": result});
                    if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("space-kit-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home/.config/herdr")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        // Short path: Unix socket paths are limited to ~100 bytes.
        let socket = std::env::temp_dir().join(format!("sk-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        fake_herdr(&socket);
        std::fs::write(
            dir.join("state/groups.json"),
            json!({"version": 2, "groups": [{"name": "Work", "color": "blue"}], "assignments": {"/space-kit-cli/api": "Work"}}).to_string(),
        )
        .unwrap();
        Self { dir, socket }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_space-kit"));
        cmd.args(args)
            .env("HOME", self.dir.join("home"))
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("HERDR_PLUGIN_STATE_DIR", self.dir.join("state"))
            .env_remove("HERDR_PLUGIN_EVENT_JSON")
            .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("SPACE_KIT_ROW_WIDTH");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn sidebar(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.dir.join("state/sidebar.json")).unwrap()).unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn sync_reports_and_toggle_collapse_folds() {
    let env = Env::new("sync");
    let out = env.run(&["sync"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "Synced 1 grouped workspace(s) across 1 group(s).\n");
    assert!(env.run(&["sync", "--quiet", "--force"]).stdout.is_empty());

    let out = env.run(&["toggle-collapse", "--workspace", "p1"]);
    assert_eq!(text(&out.stdout), "Work: collapsed\n");
    let out = env
        .command(&["toggle-collapse"])
        .env("HERDR_PLUGIN_CONTEXT_JSON", r#"{"workspace_id":"p1"}"#)
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "Work: expanded\n");
    let out = env.command(&["toggle-collapse"]).env("HERDR_WORKSPACE_ID", "h1").output().unwrap();
    assert_eq!(text(&out.stdout), "Work: collapsed\n", "the header workspace toggles its group");
}

#[test]
fn on_focus_unfolds_a_clicked_header_and_skips_other_focus_changes() {
    let env = Env::new("focus");
    env.run(&["sync", "--quiet"]);
    let focus = |id: &str| {
        let event = format!(r#"{{"event":"workspace.focused","data":{{"workspace_id":"{id}"}}}}"#);
        env.command(&["sync", "--on-focus"]).env("HERDR_PLUGIN_EVENT_JSON", event).output().unwrap()
    };
    let out = focus("p9");
    assert!(out.status.success() && out.stdout.is_empty(), "nothing folded: no sync");
    assert_eq!(env.sidebar()["collapsed"], json!([]));
    env.run(&["toggle-collapse", "--workspace", "h1"]);
    assert_eq!(env.sidebar()["collapsed"], json!(["Work"]));
    let out = focus("h1");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(env.sidebar()["collapsed"], json!(["Work"]), "by default a click keeps the fold");
    let mut sidebar = env.sidebar();
    sidebar["unfold_on_click"] = json!(true);
    std::fs::write(env.dir.join("state/sidebar.json"), sidebar.to_string()).unwrap();
    focus("h1");
    assert_eq!(env.sidebar()["collapsed"], json!([]), "with unfold_on_click, a header click unfolds");
    focus("h1");
    assert_eq!(env.sidebar()["collapsed"], json!([]), "and never folds it");
}

#[test]
fn record_caches_rate_limits_and_starts_a_sync() {
    let env = Env::new("record");
    let input = json!({"rate_limits": {"five_hour": {"used_percentage": 12}, "seven_day": {"used_percentage": 30}}})
        .to_string();
    let mut child = env.command(&["record"]).stdin(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    let cache = std::fs::read_to_string(env.dir.join("home/.cache/claude-rate-limits.json")).unwrap();
    assert!(cache.contains("\"used_percentage\":12"), "{cache}");
    // The background sync sets the Agents title from the new figures.
    let label = env.dir.join("state/view-label.txt");
    for _ in 0..50 {
        if std::fs::read_to_string(&label).is_ok_and(|l| l == "✻12% 30%") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the background sync never set the title: {:?}", std::fs::read_to_string(&label));
}

#[test]
fn open_asks_herdr_for_the_manager_pane() {
    let env = Env::new("open");
    assert!(env.command(&["open"]).env("HERDR_BIN_PATH", "true").output().unwrap().status.success());
    let out = env.command(&["open"]).env("HERDR_BIN_PATH", "false").output().unwrap();
    assert!(text(&out.stderr).starts_with("space-kit: Herdr could not open the group manager"));
    let out = env.command(&["open"]).env("HERDR_BIN_PATH", "/nonexistent/herdr").output().unwrap();
    assert!(text(&out.stderr).starts_with("space-kit: herdr:"));
}

#[test]
fn bad_arguments_fail_with_the_usage() {
    let env = Env::new("args");
    for (args, start) in [
        (&[][..], "space-kit: usage: space-kit"),
        (&["bogus"][..], "space-kit: unknown command bogus"),
        (&["sync", "--fast"][..], "space-kit: unknown argument --fast"),
        (&["toggle-collapse", "p1"][..], "space-kit: usage:"),
        (&["toggle-collapse"][..], "space-kit: no workspace to toggle"),
    ] {
        let out = env.run(args);
        assert!(!out.status.success() && text(&out.stderr).starts_with(start), "{args:?}: {}", text(&out.stderr));
    }
    assert!(text(&env.run(&["--help"]).stdout).starts_with("usage: space-kit"));
}

#[test]
fn errors_without_herdr_are_reported() {
    let env = Env::new("down");
    let out = env.command(&["sync"]).env("HERDR_SOCKET_PATH", env.dir.join("missing.sock")).output().unwrap();
    assert!(!out.status.success() && text(&out.stderr).starts_with("space-kit: connect "), "{}", text(&out.stderr));
    // Without the socket variable it falls back to ~/.config/herdr/herdr.sock under HOME.
    let out = env.command(&["sync"]).env_remove("HERDR_SOCKET_PATH").output().unwrap();
    assert!(text(&out.stderr).contains("home/.config/herdr/herdr.sock"), "{}", text(&out.stderr));
}

#[test]
fn the_state_dir_falls_back_to_xdg_then_home() {
    let env = Env::new("xdg");
    let xdg = env.dir.join("xdg");
    let out = env
        .command(&["sync", "--quiet"])
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .env("XDG_STATE_HOME", &xdg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(xdg.join("herdr/plugins/space-kit/sidebar.json").exists());
    let out = env
        .command(&["sync", "--quiet"])
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(env.dir.join("home/.local/state/herdr/plugins/space-kit/sidebar.json").exists());
}

/// `space-kit manage` in a real pseudo-terminal (via `script`), quit with `q`.
#[test]
fn the_manager_runs_in_a_terminal_and_quits() {
    let env = Env::new("manage");
    let exe = env!("CARGO_BIN_EXE_space-kit");
    let mut script = Command::new("script");
    if cfg!(target_os = "macos") {
        script.args(["-q", "/dev/null", exe, "manage"]);
    } else {
        script.args(["-qec", &format!("{exe} manage"), "/dev/null"]);
    }
    let mut child =
        env_of(&env, script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    stdin.write_all(b"q").unwrap();
    let out = child.wait_with_output().unwrap();
    let screen = text(&out.stdout);
    assert!(out.status.success(), "{screen}\n{}", text(&out.stderr));
    // `script` gives the pty no size, so nothing is drawn; check the terminal was set up and
    // restored: alternate screen and mouse capture on, then off.
    let (on, off) = (screen.find("\x1b[?1049h"), screen.rfind("\x1b[?1049l"));
    assert!(on.is_some() && off > on, "{screen:?}");
    assert!(screen.contains("\x1b[?1000h") && screen.contains("\x1b[?1000l"), "{screen:?}");
}

/// The environment of `Env::command`, applied to another program.
fn env_of(env: &Env, mut cmd: Command) -> Command {
    let template = env.command(&[]);
    for (key, value) in template.get_envs() {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    cmd.env("TERM", "xterm-256color");
    cmd
}

#[test]
fn record_without_a_change_does_not_sync_and_row_width_can_be_set() {
    let env = Env::new("width");
    let same = r#"{"rate_limits": {"five_hour": {"used_percentage": 1}}}"#;
    for _ in 0..2 {
        let mut child = env.command(&["record"]).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(same.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }
    // The first record started a background sync: let it write and release the lock, or ours would
    // only mark it dirty and the background one would re-run without the width.
    let reported = env.dir.join("state/reported.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reported.exists() {
        assert!(std::time::Instant::now() < deadline, "the background sync never ran");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::File::open(env.dir.join("state/sync.lock")).unwrap().lock().unwrap();
    let out = env.command(&["sync", "--quiet"]).env("SPACE_KIT_ROW_WIDTH", "12").output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let reported = std::fs::read_to_string(reported).unwrap();
    assert!(reported.contains("━━ Work · 1 ━"), "{reported}");
}
