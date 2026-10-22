//! The definition of done, checked with real processes. Each test has its
//! own state directory and project directory, and stops what it started when it ends.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

struct Env {
    state: tempfile::TempDir,
    projects: Vec<tempfile::TempDir>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Env {
    fn new() -> Env {
        let mut e = Env {
            state: tempfile::tempdir().unwrap(),
            projects: vec![],
        };
        e.add_project();
        e
    }

    /// A new project directory (a git top level), usable with `run_in`.
    fn add_project(&mut self) -> usize {
        let p = tempfile::tempdir().unwrap();
        std::fs::create_dir(p.path().join(".git")).unwrap();
        self.projects.push(p);
        self.projects.len() - 1
    }

    fn run_in(&self, project: usize, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_devsrv"))
            .args(args)
            .current_dir(self.projects[project].path())
            .env("DEVSRV_STATE_DIR", self.state.path())
            .output()
            .unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into(),
            stderr: String::from_utf8_lossy(&out.stderr).into(),
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_in(0, args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.run(&a).stdout).unwrap()
    }

    /// The `ls --json` entry of a name, or null when there is none (yet).
    fn process(&self, name: &str) -> Value {
        let ls = self.json(&["ls"]);
        ls["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn record_path(&self, name: &str) -> PathBuf {
        let dir = std::fs::read_dir(self.state.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        dir.join(name).join("record.json")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for i in 0..self.projects.len() {
            self.run_in(i, &["stop", "--all"]);
        }
    }
}

fn http_get(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("http://")?;
    let hostport = rest.split('/').next()?;
    let mut s = TcpStream::connect(hostport).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.split_whitespace().nth(1)?.parse().ok()
}

/// Is any process of this group alive? Asks `ps`, not devsrv.
fn group_alive(pgid: i64) -> bool {
    let out = Command::new("ps")
        .args(["-A", "-o", "pgid=,stat="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let mut f = l.split_whitespace();
        f.next().and_then(|p| p.parse::<i64>().ok()) == Some(pgid)
            && !f.next().unwrap_or("").starts_with('Z')
    })
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success()
}

/// A process devsrv did not start, leading its own group so that a wrong group signal
/// would reach it. Killed when the test ends, pass or fail.
struct Stranger(std::process::Child);

impl Stranger {
    fn new() -> Stranger {
        use std::os::unix::process::CommandExt;
        Stranger(
            Command::new("sleep")
                .arg("300")
                .process_group(0)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn eventually(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

const SERVER: &[&str] = &[
    "--port",
    "auto",
    "--",
    "python3",
    "-m",
    "http.server",
    "{port}",
];

fn start_server(env: &Env, name: &str) -> String {
    let mut args = vec!["start", name];
    args.extend_from_slice(SERVER);
    let r = env.run(&args);
    assert_eq!(r.code, 0, "{}", r.stderr);
    r.stdout
}

#[test]
fn c01_start_returns_a_reachable_url_on_one_line() {
    let env = Env::new();
    let out = start_server(&env, "web");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    let url = out.trim();
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with('/'),
        "{url}"
    );
    assert_eq!(http_get(url), Some(200));
}

#[test]
fn c02_a_command_that_dies_at_once_fails_with_its_code_and_log() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "api",
        "--",
        "sh",
        "-c",
        "echo 'config.toml: no such file' >&2; exit 3",
    ]);
    assert_eq!(r.code, 1);
    assert_eq!(r.stdout, "");
    let first = r.stderr.lines().next().unwrap();
    assert!(first.contains("api exited with code 3"), "{}", r.stderr);
    assert!(
        r.stderr.contains("| config.toml: no such file"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("hint:"), "{}", r.stderr);
}

#[test]
fn c03_a_second_start_fails_and_says_what_runs() {
    let env = Env::new();
    let url = start_server(&env, "web");
    let mut args = vec!["start", "web"];
    args.extend_from_slice(SERVER);
    let r = env.run(&args);
    assert_eq!(r.code, 1);
    let first = r.stderr.lines().next().unwrap();
    assert!(
        first.contains("web is already started")
            && first.contains(url.trim())
            && first.contains(" up "),
        "{first}"
    );
    assert!(r.stderr.contains("devsrv stop web"), "{}", r.stderr);
    assert_eq!(env.run(&["stop", "web"]).code, 0);
    assert_eq!(env.run(&args).code, 0, "starts again after stop");
}

#[test]
fn c03_a_dead_record_is_cleaned_up_and_started_over() {
    let env = Env::new();
    let r = env.run(&["start", "short", "--", "sh", "-c", "sleep 0.7"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(eventually(Duration::from_secs(5), || env.process("short")
        ["state"]
        == "dead"));
    let r = env.run(&["start", "short", "--", "sleep", "300"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(env.process("short")["state"], "ready");
}

#[test]
fn c04_stop_takes_the_grandchildren_and_twice_is_fine() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "tree",
        "--",
        "sh",
        "-c",
        "sleep 300 & sleep 301 & wait",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let pgid = env.process("tree")["pid"].as_i64().unwrap();
    assert!(group_alive(pgid));
    let r = env.run(&["stop", "tree"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("tree: stopped (TERM"), "{}", r.stdout);
    assert!(!group_alive(pgid), "the whole group is gone");
    let r = env.run(&["stop", "tree"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("already stopped"), "{}", r.stdout);
}

#[test]
fn c04_a_group_that_ignores_term_gets_kill() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "deaf",
        "--",
        "sh",
        "-c",
        "trap '' TERM; sleep 300 & wait",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let pgid = env.process("deaf")["pid"].as_i64().unwrap();
    let r = env.run(&["stop", "deaf"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("deaf: stopped (KILL"), "{}", r.stdout);
    assert!(!group_alive(pgid));
}

#[test]
fn c05_a_reused_pid_is_not_signalled() {
    let env = Env::new();
    assert_eq!(env.run(&["start", "x", "--", "sleep", "300"]).code, 0);
    let path = env.record_path("x");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // Kill the supervisor and the command behind devsrv's back...
    let sup = record["state"]["up"]["supervisor"]["pid"].as_i64().unwrap();
    let child = record["state"]["up"]["child"]["pid"].as_i64().unwrap();
    Command::new("kill")
        .args(["-KILL", &sup.to_string()])
        .status()
        .unwrap();
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{child}")])
        .status()
        .unwrap();
    assert!(eventually(Duration::from_secs(5), || !group_alive(child)));
    // ...and point the record at someone else's process, as if the pid were reused.
    let mut stranger = Stranger::new();
    record["state"]["up"]["child"]["pid"] = stranger.0.id().into();
    std::fs::write(&path, record.to_string()).unwrap();
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("x")["state"] == "orphan"
    ));
    let r = env.run(&["stop", "x"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("already stopped"), "{}", r.stdout);
    assert!(
        stranger.0.try_wait().unwrap().is_none(),
        "the stranger still runs"
    );
    assert!(pid_alive(stranger.0.id()));
}

#[test]
fn c06_wait_times_out_on_a_free_port_and_succeeds_on_a_server() {
    let env = Env::new();
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let t = Instant::now();
    let r = env.run(&["wait", "port", &free.to_string(), "--timeout", "1"]);
    let took = t.elapsed();
    assert_eq!(r.code, 1);
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert!(r.stderr.contains("not ready within 1.0s"), "{}", r.stderr);
    let url = start_server(&env, "web");
    let r = env.run(&["wait", url.trim(), "--timeout", "5"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("ready after "), "{}", r.stdout);
}

#[test]
fn c07_who_names_devsrv_processes_and_only_shows_others() {
    let env = Env::new();
    let url = start_server(&env, "web");
    let port = url
        .trim()
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    let r = env.run(&["who", &port]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout
            .starts_with(&format!("{port}: web (devsrv, this project)")),
        "{}",
        r.stdout
    );
    let mine = TcpListener::bind("127.0.0.1:0").unwrap();
    let other = mine.local_addr().unwrap().port().to_string();
    let r = env.run(&["who", &other]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains(&format!("pid {}", std::process::id())),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("not started by devsrv"), "{}", r.stdout);
    assert!(
        TcpStream::connect(mine.local_addr().unwrap()).is_ok(),
        "still listening"
    );
    drop(mine);
    let r = env.run(&["who", &other]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("free"), "{}", r.stderr);
}

#[test]
fn c08_ls_shows_alive_and_dead_in_text_and_json() {
    let env = Env::new();
    start_server(&env, "web");
    env.run(&["start", "api", "--", "sh", "-c", "exit 3"]);
    let r = env.run(&["ls"]);
    assert_eq!(r.code, 0);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert!(
        lines[0].starts_with("1 running, 1 dead in "),
        "{}",
        r.stdout
    );
    assert!(
        lines.iter().any(|l| l.starts_with("web")
            && l.contains("ready")
            && l.contains("http://127.0.0.1:")),
        "{}",
        r.stdout
    );
    assert!(
        lines.iter().any(|l| l.starts_with("api")
            && l.contains("dead")
            && l.contains("exited with code 3")),
        "{}",
        r.stdout
    );
    let web = env.process("web");
    assert_eq!(web["alive"], true);
    assert!(web["pid"].as_i64().unwrap() > 0 && web["port"].as_u64().unwrap() > 0);
    assert!(web["age_s"].as_f64().is_some() && web["cwd"].as_str().is_some());
    let api = env.process("api");
    assert_eq!(api["alive"], false);
    assert_eq!(api["end"]["exited"]["status"]["code"], 3);
}

#[test]
fn c09_logs_tail_and_follow_until() {
    let env = Env::new();
    let script = "for i in 1 2 3 4 5 6 7; do echo line$i; done; sleep 300";
    let r = env.run(&[
        "start",
        "talk",
        "--ready",
        "log:line7",
        "--",
        "sh",
        "-c",
        script,
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = env.run(&["logs", "talk", "-n", "5"]);
    assert_eq!(r.stdout, "line3\nline4\nline5\nline6\nline7\n");
    let r = env.run(&[
        "logs",
        "talk",
        "--follow-until",
        "line[5]",
        "--timeout",
        "2",
    ]);
    assert_eq!((r.code, r.stdout.as_str()), (0, "line5\n"));
    let r = env.run(&["logs", "talk", "--follow-until", "never", "--timeout", "1"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr
            .contains("no line of talk's log matched within 1.0s"),
        "{}",
        r.stderr
    );
}

#[test]
fn c10_the_same_name_in_two_projects_does_not_collide() {
    let mut env = Env::new();
    let other = env.add_project();
    let a = env.run(&["start", "web", "--", "sleep", "300"]);
    let b = env.run_in(other, &["start", "web", "--", "sleep", "301"]);
    assert_eq!((a.code, b.code), (0, 0), "{} {}", a.stderr, b.stderr);
    assert_ne!(a.stdout, b.stdout);
    assert_eq!(env.run(&["stop", "web"]).code, 0);
    let ls = env.run_in(other, &["ls"]);
    assert!(
        ls.stdout
            .lines()
            .any(|l| l.starts_with("web") && l.contains("ready")),
        "{}",
        ls.stdout
    );
}

#[test]
fn c11_the_summary_of_the_help_says_how_to_use_it_in_40_lines() {
    let env = Env::new();
    for args in [&["--help"][..], &["-h"], &["help"], &[]] {
        let r = env.run(args);
        assert_eq!(r.code, 0, "{args:?}");
        let n = r.stdout.lines().count();
        assert!(n <= 40, "`devsrv {args:?}` prints {n} lines:\n{}", r.stdout);
        assert!(r.stdout.contains("devsrv help all"), "{}", r.stdout);
    }
    let r = env.run(&["--help"]);
    let head: String = r.stdout.lines().take(40).collect::<Vec<_>>().join("\n");
    for needle in [
        "Usage:",
        "devsrv start NAME",
        "devsrv stop",
        "devsrv wait",
        "devsrv who",
        "devsrv logs",
        "devsrv ls",
        "{port}",
        "Output:",
        "--json",
        "Exit status: 0",
    ] {
        assert!(head.contains(needle), "missing {needle}");
    }
}

const HELP_TXT: &str = include_str!("../src/help.txt");

/// The headings of help.txt: lines not indented, after the summary.
fn help_headings() -> Vec<&'static str> {
    let lines: Vec<&str> = HELP_TXT.lines().collect();
    let exit = lines
        .iter()
        .position(|l| l.starts_with("Exit status:"))
        .unwrap();
    let blank = exit + lines[exit..].iter().position(|l| l.is_empty()).unwrap();
    let mut out = Vec::new();
    for i in blank + 1..lines.len() {
        if !lines[i].is_empty() && !lines[i].starts_with(' ') {
            assert!(
                lines[i - 1].is_empty(),
                "help.txt: `{}` is not indented but follows a line; a heading needs a blank line before it",
                lines[i]
            );
            out.push(lines[i]);
        }
    }
    out
}

#[test]
fn c11_help_for_a_command_has_its_section_and_not_the_others() {
    let env = Env::new();
    let r = env.run(&["help", "start"]);
    assert_eq!(r.code, 0);
    assert!(
        r.stdout.contains("devsrv start NAME [--port"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\nstart: ready checks"), "{}", r.stdout);
    for other in ["\nstop: ", "\nwho: ", "\nlogs: ", "Examples:"] {
        assert!(!r.stdout.contains(other), "{other}: {}", r.stdout);
    }
    assert!(r.stdout.contains("devsrv help state"), "{}", r.stdout);
    let r = env.run(&["help", "state"]);
    assert!(r.stdout.starts_with("state: "), "{}", r.stdout);
}

#[test]
fn c11_command_dash_dash_help_is_help_for_the_command() {
    let env = Env::new();
    for name in ["start", "ls", "logs", "stop", "who", "wait", "run", "ps"] {
        let want = env.run(&["help", name]);
        assert_eq!(want.code, 0, "{name}");
        for flag in ["--help", "-h"] {
            let got = env.run(&[name, flag]);
            assert_eq!((got.code, &got.stdout), (0, &want.stdout), "{name} {flag}");
        }
    }
}

#[test]
fn c11_help_all_is_the_whole_of_help_txt() {
    let env = Env::new();
    let r = env.run(&["help", "all"]);
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, HELP_TXT);
}

#[test]
fn c11_help_for_an_unknown_name_says_so_and_lists_the_names() {
    let env = Env::new();
    let r = env.run(&["help", "foo"]);
    assert_eq!(r.code, 2);
    assert!(r.stdout.is_empty());
    let first = r.stderr.lines().next().unwrap();
    assert!(
        first.contains("`foo`") && first.contains("start") && first.contains("spelling"),
        "{}",
        r.stderr
    );
    assert_eq!(env.run(&["foo", "--help"]).code, 2);
    assert_eq!(env.run(&["help", "a", "b"]).code, 2);
}

/// The words after `--` are the command's own: its `--help` is not devsrv's.
#[test]
fn c11_help_after_dash_dash_belongs_to_the_command() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "h",
        "--ready",
        "none",
        "--",
        "sh",
        "-c",
        r#"printf 'got:%s\n' "$@"; sleep 30"#,
        "sh",
        "--help",
        "-h",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("pid "), "{}", r.stdout);
    let log = env.run(&["logs", "h"]);
    assert!(
        log.stdout.contains("got:--help") && log.stdout.contains("got:-h"),
        "{}",
        log.stdout
    );
    assert_eq!(env.run(&["stop", "h"]).code, 0);
}

/// Without `--`, the command begins at its first word: what follows is its own too.
#[test]
fn c11_help_and_json_in_a_command_without_dash_dash_are_the_commands() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "h",
        "--ready",
        "none",
        "sh",
        "-c",
        r#"printf 'got:%s\n' "$@"; sleep 30"#,
        "sh",
        "-h",
        "--json",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("pid "), "{}", r.stdout);
    let log = env.run(&["logs", "h"]);
    assert!(
        log.stdout.contains("got:-h") && log.stdout.contains("got:--json"),
        "{}",
        log.stdout
    );
    assert_eq!(env.run(&["stop", "h"]).code, 0);
    // `false -h` is run, and fails; devsrv's help is not a success.
    let r = env.run(&["wait", "--timeout", "300ms", "cmd", "false", "-h", "x"]);
    assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
    assert!(r.stderr.contains("`false -h x`: not ready"), "{}", r.stderr);
    let r = env.run(&["wait", "-t", "300ms", "cmd", "false", "--json"]);
    assert_eq!(r.code, 1, "{}{}", r.stdout, r.stderr);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    // Before the command they are still devsrv's.
    let r = env.run(&["wait", "-t", "300ms", "--json", "cmd", "false"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&r.stdout).unwrap()["exit_code"],
        1
    );
    let r = env.run(&["start", "h", "--port", "auto", "--help", "x", "{port}"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("devsrv start NAME"), "{}", r.stdout);
}

/// A section of help.txt that no name asks for would be written but never read.
#[test]
fn c11_every_section_of_help_txt_is_asked_for_by_some_name() {
    let env = Env::new();
    let r = env.run(&["help", "foo"]);
    let list = r
        .stderr
        .lines()
        .next()
        .unwrap()
        .split("try: ")
        .nth(1)
        .unwrap();
    let names: Vec<&str> = list.split(", ").filter(|n| *n != "all").collect();
    assert!(names.len() >= 6, "{names:?}");
    let mut shown = String::new();
    for name in names {
        let r = env.run(&["help", name]);
        assert_eq!(r.code, 0, "{name}");
        shown.push_str(&r.stdout);
    }
    let headings = help_headings();
    assert!(headings.len() >= 6, "{headings:?}");
    for h in headings {
        assert!(
            shown.lines().any(|l| l == h),
            "no name of `devsrv help` prints the section `{h}`"
        );
    }
}

#[test]
fn c12_the_command_outlives_the_shell_that_started_it() {
    let env = Env::new();
    let devsrv = env!("CARGO_BIN_EXE_devsrv");
    let out = Command::new("sh")
        .args([
            "-c",
            &format!("'{devsrv}' start web --port auto -- python3 -m http.server {{port}}; exit 0"),
        ])
        .current_dir(env.projects[0].path())
        .env("DEVSRV_STATE_DIR", env.state.path())
        .output()
        .unwrap();
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(http_get(&url), Some(200), "{url}");
}

#[test]
fn a_fixed_port_in_use_is_refused_with_its_owner() {
    let env = Env::new();
    let mine = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = mine.local_addr().unwrap().port().to_string();
    let r = env.run(&[
        "start",
        "web",
        "--port",
        &port,
        "--",
        "python3",
        "-m",
        "http.server",
        "{port}",
    ]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains(&format!(
            "port {port} is in use by pid {}",
            std::process::id()
        )),
        "{}",
        r.stderr
    );
    assert!(
        env.run(&["ls"]).stdout.starts_with("nothing started"),
        "nothing was started"
    );
}

#[test]
fn a_command_that_cannot_be_run_is_an_input_error() {
    let env = Env::new();
    let r = env.run(&["start", "x", "--", "no-such-command-devsrv"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stderr.contains("cannot run `no-such-command-devsrv`"),
        "{}",
        r.stderr
    );
}

#[test]
fn not_ready_in_time_stops_the_command_and_explains() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "deaf",
        "--port",
        "auto",
        "--timeout",
        "1",
        "--",
        "sleep",
        "300",
    ]);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.lines().next().unwrap().contains(
            "deaf was not ready within 1.0s (last check: connection refused); stopped it"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("{port}"),
        "hints at {{port}}: {}",
        r.stderr
    );
    assert_eq!(env.process("deaf")["state"], "dead");
}

#[test]
fn json_failures_go_to_stdout_with_the_exit_code() {
    let env = Env::new();
    let r = env.run(&["--json", "stop", "nope"]);
    assert_eq!(r.code, 1);
    assert_eq!(r.stderr, "");
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        (v["ok"].clone(), v["error"].clone(), v["exit_code"].clone()),
        (false.into(), "unknown_name".into(), 1.into())
    );
}

#[test]
fn an_unwritable_state_directory_fails_before_running_anything() {
    let env = Env::new();
    let marker = env.projects[0].path().join("ran");
    let out = Command::new(env!("CARGO_BIN_EXE_devsrv"))
        .args(["start", "x", "--", "touch", marker.to_str().unwrap()])
        .current_dir(env.projects[0].path())
        .env("DEVSRV_STATE_DIR", Path::new("/dev/null/devsrv"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("export DEVSRV_STATE_DIR="));
    assert!(!marker.exists());
}

fn signal(sig: &str, pid: i64) {
    Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap();
}

fn record_of(env: &Env, name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(env.record_path(name)).unwrap()).unwrap()
}

fn devsrv_child(env: &Env, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_devsrv"))
        .args(args)
        .current_dir(env.projects[0].path())
        .env("DEVSRV_STATE_DIR", env.state.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn a_start_does_not_report_a_later_run_as_its_own() {
    let env = Env::new();
    let first = devsrv_child(
        &env,
        &[
            "start",
            "x",
            "--ready",
            "log:never",
            "--",
            "sh",
            "-c",
            "sleep 0.5; exit 3",
        ],
    );
    assert!(eventually(Duration::from_secs(5), || env
        .state
        .path()
        .read_dir()
        .unwrap()
        .next()
        .is_some()
        && env.record_path("x").exists()));
    signal("-STOP", first.id() as i64);
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("x")["state"] == "dead"
    ));
    assert_eq!(env.run(&["start", "x", "--", "sleep", "300"]).code, 0);
    signal("-CONT", first.id() as i64);
    let out = first.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
}

#[test]
fn a_stop_finishes_the_job_when_the_supervisor_dies_midway() {
    let env = Env::new();
    assert_eq!(env.run(&["start", "x", "--", "sleep", "300"]).code, 0);
    let rec = record_of(&env, "x");
    let sup = rec["state"]["up"]["supervisor"]["pid"].as_i64().unwrap();
    let pgid = rec["state"]["up"]["child"]["pid"].as_i64().unwrap();
    signal("-STOP", sup);
    let stop = devsrv_child(&env, &["stop", "x"]);
    let stop_file = env.record_path("x").with_file_name("stop");
    assert!(eventually(Duration::from_secs(5), || stop_file.exists()));
    signal("-KILL", sup);
    let out = stop.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("its supervisor was gone"));
    assert!(
        !group_alive(pgid),
        "the command was stopped, not just reported"
    );
}

#[test]
fn a_log_line_left_by_a_command_that_exited_is_not_ready() {
    let env = Env::new();
    // The line and the exit both land between two probes (50 ms apart), and the
    // grandchild ignores TERM, so a wrong "ready" would stay visible for the 5 s grace.
    let script = "trap '' TERM; sleep 300 & sleep 0.2; echo READY; exit 3";
    let r = env.run(&[
        "start",
        "x",
        "--ready",
        "log:READY",
        "--",
        "sh",
        "-c",
        script,
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("x exited with code 3"), "{}", r.stderr);
}

#[test]
fn an_http_reply_that_trickles_does_not_stretch_the_timeout() {
    let env = Env::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            std::thread::spawn(move || {
                for _ in 0..50 {
                    if s.write_all(b"x").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
    });
    let t = Instant::now();
    let r = env.run(&[
        "wait",
        &format!("http://127.0.0.1:{port}/"),
        "--timeout",
        "300ms",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn stop_all_reports_a_broken_record_instead_of_skipping_it() {
    let env = Env::new();
    assert_eq!(env.run(&["start", "x", "--", "sleep", "300"]).code, 0);
    let path = env.record_path("x");
    let good = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let r = env.run(&["stop", "--all"]);
    assert_eq!(r.code, 3, "{}", r.stdout);
    assert!(r.stderr.contains("broken"), "{}", r.stderr);
    std::fs::write(&path, good).unwrap();
}

#[test]
fn an_orphan_that_still_runs_is_shown_as_alive_and_as_devsrvs() {
    let env = Env::new();
    let url = start_server(&env, "web");
    let port = url
        .trim()
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    let sup = record_of(&env, "web")["state"]["up"]["supervisor"]["pid"]
        .as_i64()
        .unwrap();
    signal("-KILL", sup);
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("web")["state"] == "orphan"
    ));
    assert_eq!(env.process("web")["alive"], true);
    assert!(env.run(&["ls"]).stdout.starts_with("1 running, 0 dead"));
    let who = env.run(&["who", &port]);
    assert!(
        who.stdout.contains("web (devsrv, this project)") && who.stdout.contains("devsrv stop web"),
        "{}",
        who.stdout
    );
    let r = env.run(&["stop", "web"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(http_get(url.trim()), None, "stopped");
}

#[test]
fn of_concurrent_starts_of_one_name_exactly_one_wins() {
    let env = Env::new();
    let runs: Vec<_> = (0..5)
        .map(|_| devsrv_child(&env, &["start", "x", "--", "sleep", "377"]))
        .collect();
    let codes: Vec<i32> = runs
        .into_iter()
        .map(|c| c.wait_with_output().unwrap().status.code().unwrap())
        .collect();
    assert_eq!(codes.iter().filter(|&&c| c == 0).count(), 1, "{codes:?}");
    assert!(codes.iter().all(|&c| c == 0 || c == 1), "{codes:?}");
    let out = Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .unwrap();
    let sleeps = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.trim() == "sleep 377")
        .count();
    assert_eq!(sleeps, 1, "the losers started nothing");
    assert_eq!(env.process("x")["state"], "ready");
}

#[test]
fn a_start_killed_midway_still_ends_ready_or_stopped() {
    let env = Env::new();
    let mut start = devsrv_child(
        &env,
        &[
            "start",
            "slow",
            "--ready",
            "log:up",
            "--",
            "sh",
            "-c",
            "sleep 0.5; echo up; sleep 300",
        ],
    );
    // Killed once its supervisor runs (the first record is there), before it is ready.
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("slow")["state"] == "starting"
    ));
    start.kill().unwrap();
    start.wait().unwrap();
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("slow")["state"] == "ready"
    ));
    assert_eq!(env.run(&["stop", "slow"]).code, 0);
}

#[test]
fn what_a_dead_leader_left_in_an_orphan_group_is_named_not_signalled() {
    let env = Env::new();
    assert_eq!(
        env.run(&["start", "tree", "--", "sh", "-c", "sleep 300 & sleep 300"])
            .code,
        0
    );
    let rec = record_of(&env, "tree");
    let sup = rec["state"]["up"]["supervisor"]["pid"].as_i64().unwrap();
    let pgid = rec["state"]["up"]["child"]["pid"].as_i64().unwrap();
    signal("-KILL", sup);
    signal("-KILL", pgid);
    assert!(eventually(
        Duration::from_secs(5),
        || env.process("tree")["alive"] == false
    ));
    let r = env.run(&["stop", "tree"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stderr.contains("devsrv cannot prove they are its own"),
        "{}",
        r.stderr
    );
    assert!(group_alive(pgid), "nothing was sent");
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .status()
        .unwrap();
    assert!(eventually(Duration::from_secs(5), || !group_alive(pgid)));
}

#[test]
fn a_server_on_ipv6_only_gets_the_address_that_answered() {
    let env = Env::new();
    let r = env.run(&[
        "start",
        "v6",
        "--port",
        "auto",
        "--",
        "python3",
        "-m",
        "http.server",
        "--bind",
        "::1",
        "{port}",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let url = r.stdout.trim();
    assert!(url.starts_with("http://[::1]:"), "{url}");
    let port = url.trim_end_matches('/').rsplit(':').next().unwrap();
    let mut s = TcpStream::connect(format!("[::1]:{port}")).unwrap();
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.0 200"), "{buf}");
}

#[test]
fn a_record_from_before_run_ids_still_reads() {
    let env = Env::new();
    env.run(&["start", "old", "--", "sh", "-c", "exit 0"]);
    let path = env.record_path("old");
    let mut rec = record_of(&env, "old");
    rec["spec"].as_object_mut().unwrap().remove("run");
    std::fs::write(&path, rec.to_string()).unwrap();
    let r = env.run(&["ls"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("old"), "{}", r.stdout);
}

/// The body of a GET, or empty.
fn http_body(url: &str) -> String {
    let hostport = url.trim_start_matches("http://").split('/').next().unwrap();
    let mut s = TcpStream::connect(hostport).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default()
}

/// What `devsrv help compare` shows: the old code from another worktree and the new one,
/// two names in one place, each on its own port, both stopped in one call.
#[test]
fn two_versions_run_side_by_side_under_two_names() {
    let mut env = Env::new();
    let old = env.add_project();
    std::fs::write(env.projects[0].path().join("index.html"), "after\n").unwrap();
    std::fs::write(env.projects[old].path().join("index.html"), "before\n").unwrap();
    let line = format!(
        "cd '{}' && exec python3 -m http.server {{port}}",
        env.projects[old].path().display()
    );
    let before = env.run(&["start", "before", "--port", "auto", "--", "sh", "-c", &line]);
    assert_eq!(before.code, 0, "{}", before.stderr);
    let mut args = vec!["start", "after"];
    args.extend_from_slice(SERVER);
    let after = env.run(&args);
    assert_eq!(after.code, 0, "{}", after.stderr);
    let (b, a) = (before.stdout.trim(), after.stdout.trim());
    assert_ne!(b, a);
    assert_eq!(http_body(b), "before\n");
    assert_eq!(http_body(a), "after\n");
    let r = env.run(&["stop", "before", "after"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    for name in ["before", "after"] {
        assert_eq!(env.process(name)["alive"], false, "{name}");
    }
}

/// Writes `text` to `path` after `delay`, from another thread.
fn write_later(path: PathBuf, delay: Duration, text: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        std::fs::write(path, text).unwrap();
    })
}

#[test]
fn wait_status_waits_until_the_page_is_no_longer_404() {
    let env = Env::new();
    let url = start_server(&env, "web");
    let page = format!("{}app.js", url.trim());
    // A plain wait takes 404 as an answer; --status 200 does not.
    let r = env.run(&["wait", &page, "--timeout", "5"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = env.run(&["wait", &page, "--status", "200", "--timeout", "300ms"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    let first = r.stderr.lines().next().unwrap();
    assert!(
        first.contains("app.js: not ready within 0.3s (last check: HTTP 404)"),
        "{first}"
    );
    assert!(r.stderr.contains("hint: --status 200"), "{}", r.stderr);
    let made = write_later(
        env.projects[0].path().join("app.js"),
        Duration::from_millis(400),
        "x",
    );
    let t = Instant::now();
    let r = env.run(&["wait", &page, "--status", "2xx", "--timeout", "5"]);
    made.join().unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        t.elapsed() >= Duration::from_millis(350),
        "{:?}",
        t.elapsed()
    );
    assert!(r.stdout.contains("(HTTP 200)"), "{}", r.stdout);
    // Only a URL has a status.
    let r = env.run(&["wait", "port", "9", "--status", "200"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    let r = env.run(&["wait", &page, "--status", "2x0"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stderr.starts_with("devsrv: invalid --status `2x0`"),
        "{}",
        r.stderr
    );
}

#[test]
fn wait_file_regex_waits_for_the_file_then_the_line() {
    let env = Env::new();
    let dir = env.projects[0].path();
    // The file is not there yet: the wait goes on, and a timeout says so.
    let r = env.run(&[
        "wait",
        "file",
        "build.log",
        "built in",
        "--timeout",
        "200ms",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.starts_with(
            "devsrv: `built in` in build.log: not ready within 0.2s (last check: no such file)"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("hint: nothing has written build.log yet"),
        "{}",
        r.stderr
    );
    let made = write_later(
        dir.join("build.log"),
        Duration::from_millis(300),
        "\x1b[32mcompiling\x1b[0m\n\x1b[1mbuilt in\x1b[0m 3ms\n",
    );
    let r = env.run(&[
        "wait",
        "file",
        "build.log",
        "^built in [0-9]+ms$",
        "-t",
        "5",
    ]);
    made.join().unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout.lines().count(), 1, "{}", r.stdout);
    assert!(r.stdout.ends_with(": built in 3ms\n"), "{}", r.stdout);
    // No line matches: the end of the file is shown, colors removed, to fix the pattern.
    let r = env.run(&[
        "wait",
        "--file",
        "build.log",
        "listening",
        "--timeout",
        "200ms",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains("last check: no line matched"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("  | compiling\n  | built in 3ms\n"),
        "{}",
        r.stderr
    );
    // The path alone still waits for the file only.
    let r = env.run(&["wait", "file", "build.log", "--timeout", "1"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    // grep is not a form; the failure says how to write it.
    let r = env.run(&["wait", "grep", "built", "build.log"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("wait file PATH REGEX"), "{}", r.stderr);
}

#[test]
fn wait_cmd_runs_until_it_succeeds() {
    let env = Env::new();
    let made = write_later(
        env.projects[0].path().join("flag"),
        Duration::from_millis(300),
        "",
    );
    let r = env.run(&["wait", "--timeout", "5", "cmd", "--", "test", "-f", "flag"]);
    made.join().unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("ready after "), "{}", r.stdout);
    assert!(r.stdout.contains("`test -f flag` exited 0"), "{}", r.stdout);
    // One quoted line runs with sh -c, and options may follow it.
    let r = env.run(&[
        "wait",
        "cmd",
        "test -f flag && test -f flag",
        "--timeout",
        "2",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
}

#[test]
fn wait_cmd_that_keeps_failing_shows_its_last_exit_and_output() {
    let env = Env::new();
    let r = env.run(&[
        "wait",
        "--timeout",
        "1200ms",
        "cmd",
        "--",
        "sh",
        "-c",
        "echo trying; echo refused >&2; exit 3",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    let first = r.stderr.lines().next().unwrap();
    assert_eq!(
        first,
        "devsrv: `echo trying; echo refused >&2; exit 3`: not ready within 1.2s (last check: exit 3)"
    );
    assert!(
        r.stderr.contains("  | trying\n  | refused\n"),
        "{}",
        r.stderr
    );
    let v: Value = serde_json::from_str(
        &env.run(&[
            "--json",
            "wait",
            "-t",
            "300ms",
            "cmd",
            "--",
            "sh",
            "-c",
            "echo no; exit 1",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(v["exit_code"], 1);
    assert_eq!(v["data"]["last"], "exit 1");
    assert_eq!(v["data"]["tail"], serde_json::json!(["no"]));
    // The second run is still going at the deadline: what is shown is the first, which
    // finished, not the one that was cut short.
    let r = env.run(&[
        "wait",
        "--timeout",
        "1s",
        "cmd",
        "--",
        "sh",
        "-c",
        "if [ -f seen ]; then sleep 5; else touch seen; echo first; exit 3; fi",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("(last check: exit 3)"), "{}", r.stderr);
    assert!(r.stderr.contains("  | first\n"), "{}", r.stderr);
    // devsrv's options after `--` are the command's; the failure says where they go.
    let r = env.run(&[
        "wait",
        "-t",
        "300ms",
        "cmd",
        "--",
        "sh",
        "-c",
        "exit 1",
        "--timeout",
        "5",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains("put devsrv's options before `cmd`"),
        "{}",
        r.stderr
    );
}

#[test]
fn wait_cmd_kills_a_run_at_the_deadline_with_its_group() {
    let env = Env::new();
    let t = Instant::now();
    let r = env.run(&[
        "wait",
        "--timeout",
        "500ms",
        "cmd",
        "--",
        "sh",
        "-c",
        "sleep 300 & echo $! > child.pid; sleep 300",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(
        r.stderr
            .contains("last check: still running at the deadline; killed it"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("hint: one run takes longer than --timeout"),
        "{}",
        r.stderr
    );
    let pid: u32 = std::fs::read_to_string(env.projects[0].path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        eventually(Duration::from_secs(2), || !pid_alive(pid)),
        "the background child {pid} of the run is still alive"
    );
}

#[test]
fn wait_cmd_not_on_path_fails_at_once_but_a_path_is_waited_for() {
    let env = Env::new();
    let t = Instant::now();
    let r = env.run(&["wait", "cmd", "--", "no-such-command-devsrv"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(
        r.stderr
            .starts_with("devsrv: cannot run `no-such-command-devsrv`: not found on PATH"),
        "{}",
        r.stderr
    );
    let r = env.run(&["wait", "-t", "300ms", "cmd", "--", "./bin/app"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains("last check: cannot run it"),
        "{}",
        r.stderr
    );
}

/// The pids a file lists, one per line.
fn pids_in(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
fn wait_cmd_leaves_nothing_of_a_run_that_ended() {
    let env = Env::new();
    let dir = env.projects[0].path();
    // Every run fails and leaves a child behind; each child goes with its run.
    let r = env.run(&[
        "wait",
        "--timeout",
        "1200ms",
        "cmd",
        "--",
        "sh",
        "-c",
        "sleep 300 & echo $! >> failed.pids; exit 1",
    ]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    // The run that succeeds, too.
    let r = env.run(&[
        "wait",
        "cmd",
        "--",
        "sh",
        "-c",
        "sleep 300 & echo $! > ok.pids; true",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let failed = pids_in(&dir.join("failed.pids"));
    assert!(failed.len() >= 2, "{failed:?}");
    for pid in failed.into_iter().chain(pids_in(&dir.join("ok.pids"))) {
        assert!(
            eventually(Duration::from_secs(2), || !pid_alive(pid)),
            "pid {pid}, left by a run, is still alive"
        );
    }
}

#[test]
fn wait_cmd_killed_by_its_caller_takes_the_running_command_along() {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let env = Env::new();
    let dir = env.projects[0].path();
    for (sig, n) in [("TERM", 15), ("INT", 2), ("HUP", 1)] {
        let file = dir.join(format!("{sig}.pid"));
        let line = format!("echo $$ > {sig}.pid; sleep 300");
        // As a tool call that times out does: the signal goes to the whole group of the
        // caller, which the run, in its own group, is not in.
        let mut waiting = Command::new(env!("CARGO_BIN_EXE_devsrv"))
            .args(["wait", "--timeout", "1m", "cmd", "--", "sh", "-c", &line])
            .current_dir(dir)
            .env("DEVSRV_STATE_DIR", env.state.path())
            .process_group(0)
            .spawn()
            .unwrap();
        assert!(
            eventually(Duration::from_secs(5), || !pids_in(&file).is_empty()),
            "{sig}: the run did not start"
        );
        let run = pids_in(&file)[0];
        Command::new("kill")
            .args([&format!("-{sig}"), "--", &format!("-{}", waiting.id())])
            .status()
            .unwrap();
        let status = waiting.wait().unwrap();
        assert_eq!(status.signal(), Some(n), "{sig}: {status:?}");
        assert!(
            eventually(Duration::from_secs(2), || !pid_alive(run)),
            "{sig}: the run {run} outlived devsrv wait"
        );
    }
}
