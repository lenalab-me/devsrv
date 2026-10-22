//! The command line. Loose in form, strict in meaning: spellings that can only mean one
//! thing are accepted (`--port=auto`, `--timeout 30s`, `http:health`), anything else is
//! exit 2 with the usage of that subcommand.

use std::path::PathBuf;
use std::time::Duration;

use regex_lite::Regex;

use crate::model::{Name, Ready};
use crate::probe::Statuses;
use crate::report::Failure;

pub enum Cmd {
    /// `help [NAME]` or `NAME --help`: the summary, or what `help::render` makes of the name.
    Help(Option<String>),
    Version,
    Supervise,
    Start(StartArgs),
    Ls {
        all: bool,
    },
    Logs {
        name: Name,
        lines: usize,
        follow: Option<Regex>,
        timeout: Duration,
    },
    Stop {
        names: Vec<Name>,
        all: bool,
    },
    Who {
        port: u16,
    },
    Wait {
        target: WaitTarget,
        timeout: Duration,
        /// `--interval`: the pause between checks, if not the default of the target.
        interval: Option<Duration>,
    },
}

pub enum PortArg {
    Auto,
    Fixed(u16),
}

pub struct StartArgs {
    pub name: Name,
    pub port: Option<PortArg>,
    pub ready: Ready,
    pub timeout: Duration,
    /// With `{port}` still in it.
    pub argv: Vec<String>,
}

pub enum WaitTarget {
    Url {
        /// As it goes in a URL: an IPv6 address in brackets.
        host: String,
        port: u16,
        path: String,
        /// `--status`; none means any final status from 200 to 499.
        statuses: Option<Statuses>,
    },
    Port(u16),
    /// The file exists; with a pattern, a line of it matches.
    File {
        path: PathBuf,
        pattern: Option<Regex>,
    },
    Log {
        name: Name,
        pattern: Regex,
    },
    Gone(i32),
    /// The command exits 0.
    Cmd(Vec<String>),
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

fn usage_of(sub: &str) -> &'static str {
    match sub {
        "start" => {
            "devsrv start NAME [--port auto|N] [--ready http[:PATH]|port|log:REGEX|none] [--timeout 30s] -- CMD ARGS..."
        }
        "ls" => "devsrv ls [--all]",
        "logs" => "devsrv logs NAME [-n 50] [--follow-until REGEX [--timeout 30s]]",
        "stop" => "devsrv stop NAME... | --all",
        "who" => "devsrv who PORT",
        _ => {
            "devsrv wait [--timeout 30s] [--interval 0.5s] URL [--status 200] | port N | file PATH [REGEX] | log NAME REGEX | gone PID | cmd -- CMD ARGS..."
        }
    }
}

fn bad(sub: &str, message: impl Into<String>) -> Failure {
    Failure::usage(message)
        .detail(format!("usage: {}", usage_of(sub)))
        .hint(format!("devsrv help {sub}"))
}

/// Splits `--opt=value` and reads the value of `--opt value`.
struct Args {
    items: Vec<String>,
    pos: usize,
    sub: &'static str,
    /// `--json` and `--help` met by `next_opt`.
    json: bool,
    help: bool,
}

impl Args {
    fn new(items: Vec<String>, sub: &'static str) -> Args {
        Args {
            items,
            pos: 1,
            sub,
            json: false,
            help: false,
        }
    }
    fn next(&mut self) -> Option<String> {
        let a = self.items.get(self.pos).cloned();
        self.pos += 1;
        a
    }
    /// The next argument where an option of devsrv may stand, with `--json` and `--help`
    /// taken out. Once the command of `start` or `wait cmd` begins, its arguments are
    /// read with `next` or `rest`, so the same words there stay the command's own.
    fn next_opt(&mut self) -> Option<String> {
        loop {
            let a = self.next()?;
            match a.as_str() {
                "--json" => self.json = true,
                "-h" | "--help" => self.help = true,
                _ => return Some(a),
            }
        }
    }
    fn peek(&self) -> Option<&str> {
        self.items.get(self.pos).map(String::as_str)
    }
    fn rest(&mut self) -> Vec<String> {
        let r = self.items[self.pos.min(self.items.len())..].to_vec();
        self.pos = self.items.len();
        r
    }
    /// The value of option `opt`, given inline (`--opt=v`) or as the next argument.
    fn value(&mut self, opt: &str, inline: Option<String>) -> Result<String, Failure> {
        inline
            .or_else(|| self.next())
            .ok_or_else(|| bad(self.sub, format!("{opt} needs a value")))
    }
}

/// `--opt=value` → (`--opt`, Some(value)). `-n5` → (`-n`, Some("5")).
fn split_opt(a: &str) -> (String, Option<String>) {
    if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
        return (k.to_string(), Some(v.to_string()));
    }
    if a.starts_with('-')
        && !a.starts_with("--")
        && a.len() > 2
        && a.as_bytes()[1].is_ascii_alphabetic()
    {
        return (a[..2].to_string(), Some(a[2..].to_string()));
    }
    (a.to_string(), None)
}

pub fn duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    let (num, scale) = if let Some(n) = t.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60.0)
    } else {
        (t, 1.0)
    };
    let v: f64 = num.trim().parse().ok()?;
    (v.is_finite() && v > 0.0 && v * scale <= 86_400.0).then(|| Duration::from_secs_f64(v * scale))
}

fn timeout(sub: &str, text: &str) -> Result<Duration, Failure> {
    duration(text).ok_or_else(|| {
        bad(
            sub,
            format!("invalid --timeout `{text}`: use seconds (30, 30s), 500ms or 2m"),
        )
    })
}

/// `200`, `200-299`, `2xx`, or several of them with commas: `200,204,3xx`. A 1xx is
/// never final, so no wait could end on one.
fn statuses(text: &str) -> Result<Statuses, Failure> {
    let one = |item: &str| -> Option<(u16, u16)> {
        let item = item.trim();
        let code = |s: &str| {
            s.trim()
                .parse::<u16>()
                .ok()
                .filter(|c| (200..=599).contains(c))
        };
        if let Some(d) = item.strip_suffix("xx").or_else(|| item.strip_suffix("XX")) {
            let d = code(&format!("{d}00"))?;
            return Some((d, d + 99));
        }
        match item.split_once('-') {
            Some((lo, hi)) => Some((code(lo)?, code(hi)?)).filter(|(lo, hi)| lo <= hi),
            None => code(item).map(|c| (c, c)),
        }
    };
    text.split(',')
        .map(one)
        .collect::<Option<Vec<_>>>()
        .map(Statuses)
        .ok_or_else(|| {
            bad(
                "wait",
                format!("invalid --status `{text}`: codes from 200 to 599, as 200, 200-299, 2xx or 200,304"),
            )
        })
}

fn port(sub: &str, text: &str) -> Result<u16, Failure> {
    text.trim_start_matches(':')
        .parse::<u16>()
        .ok()
        .filter(|&p| p > 0)
        .ok_or_else(|| {
            bad(
                sub,
                format!("invalid port `{text}`: a number from 1 to 65535"),
            )
        })
}

fn name(sub: &str, text: Option<String>) -> Result<Name, Failure> {
    let text = text.ok_or_else(|| bad(sub, "missing NAME"))?;
    Name::parse(&text).ok_or_else(|| {
        bad(sub, format!("invalid name `{text}`: a letter, then letters, digits, `.`, `_` or `-` (64 at most)"))
    })
}

fn regex(sub: &str, text: &str) -> Result<Regex, Failure> {
    if text.is_empty() {
        return Err(bad(sub, "the pattern is empty"));
    }
    Regex::new(text).map_err(|e| bad(sub, format!("invalid pattern `{text}`: {e}")))
}

pub fn ready(text: &str) -> Result<Ready, Failure> {
    let t = text.trim();
    if t == "http" {
        return Ok(Ready::Http { path: "/".into() });
    }
    if let Some(p) = t.strip_prefix("http:").filter(|p| !p.starts_with("//")) {
        let path = if p.starts_with('/') {
            p.to_string()
        } else {
            format!("/{p}")
        };
        if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("start", format!("invalid --ready path `{p}`")));
        }
        return Ok(Ready::Http { path });
    }
    if let Some(p) = t.strip_prefix("log:") {
        regex("start", p)?;
        return Ok(Ready::Log {
            pattern: p.to_string(),
        });
    }
    match t {
        "port" | "tcp" => Ok(Ready::Port),
        "none" => Ok(Ready::Settle),
        _ => Err(bad(
            "start",
            format!("invalid --ready `{text}`: http, http:/PATH, port, log:REGEX or none"),
        )),
    }
}

/// Parses the arguments after the program name.
pub fn parse(raw: Vec<String>) -> (bool, Result<Cmd, Failure>) {
    // `start` and `wait cmd` run a command, which begins at `--` or at its first word
    // (`start web node a.js --help`): its `--help` and `--json` are its own. Their parsers
    // take devsrv's flags only where an option of theirs may stand.
    let lead = raw
        .iter()
        .take_while(|a| a.starts_with('-') && a.as_str() != "--")
        .count();
    let sub: Option<&'static str> = match raw.get(lead).map(|w| crate::help::canonical(w)) {
        Some("start") => Some("start"),
        Some("wait") => Some("wait"),
        _ => None,
    };
    if let Some(sub) = sub {
        let word = raw[lead].clone();
        let flags = &raw[..lead];
        let json = flags.iter().any(|a| a == "--json");
        let help = flags.iter().any(|a| a == "-h" || a == "--help");
        if !help && flags.iter().any(|a| a == "-V" || a == "--version") {
            return (json, Ok(Cmd::Version));
        }
        let mut a = Args::new(raw[lead..].to_vec(), sub);
        let cmd = if sub == "start" {
            start(&mut a)
        } else {
            wait(&mut a)
        };
        if help || a.help {
            return (json || a.json, Ok(Cmd::Help(Some(word))));
        }
        return (json || a.json, cmd);
    }
    // The other commands run nothing: --json, --help and --version count anywhere
    // before `--`.
    let dashdash = raw.iter().position(|a| a == "--").unwrap_or(raw.len());
    let json = raw[..dashdash].iter().any(|a| a == "--json");
    let items: Vec<String> = raw
        .iter()
        .enumerate()
        .filter(|(i, a)| *i >= dashdash || a.as_str() != "--json")
        .map(|(_, a)| a.clone())
        .collect();
    let dashdash = items.iter().position(|a| a == "--").unwrap_or(items.len());
    let before = &items[..dashdash];
    let help = before.iter().any(|a| a == "-h" || a == "--help");
    if help || items.is_empty() || items[0] == "help" {
        let mut names = before
            .iter()
            .filter(|a| !a.starts_with('-'))
            .filter(|a| a.as_str() != "help");
        let name = names.next().cloned();
        let extra = names.next().filter(|_| items[0] == "help");
        if let (Some(_), Some(extra)) = (&name, extra) {
            return (
                json,
                Err(
                    Failure::usage(format!("help takes one name, not `{extra}`"))
                        .hint("devsrv help COMMAND"),
                ),
            );
        }
        return (json, Ok(Cmd::Help(name)));
    }
    if items[0] == "-V" || items[0] == "--version" {
        return (json, Ok(Cmd::Version));
    }
    (json, parse_sub(items))
}

fn parse_sub(items: Vec<String>) -> Result<Cmd, Failure> {
    let sub: &'static str = match crate::help::canonical(&items[0]) {
        "start" => "start",
        "ls" => "ls",
        "logs" => "logs",
        "stop" => "stop",
        "who" => "who",
        "wait" => "wait",
        "__supervise" => return Ok(Cmd::Supervise),
        other => {
            return Err(Failure::usage(format!("unknown command `{other}`"))
                .detail("commands: start, ls, logs, stop, who, wait")
                .hint("devsrv --help"));
        }
    };
    let mut a = Args::new(items, sub);
    match sub {
        "start" => start(&mut a),
        "ls" => {
            let mut all = false;
            while let Some(x) = a.next() {
                match x.as_str() {
                    "--all" | "-a" => all = true,
                    _ => return Err(bad(sub, format!("unexpected `{x}`"))),
                }
            }
            Ok(Cmd::Ls { all })
        }
        "logs" => logs(&mut a),
        "stop" => {
            let (mut names, mut all) = (vec![], false);
            while let Some(x) = a.next() {
                match x.as_str() {
                    "--all" | "-a" => all = true,
                    _ => names.push(name(sub, Some(x))?),
                }
            }
            if names.is_empty() == !all {
                return Err(bad(
                    sub,
                    if all {
                        "give names or --all, not both"
                    } else {
                        "missing NAME (or --all)"
                    },
                ));
            }
            Ok(Cmd::Stop { names, all })
        }
        "who" => {
            let p = a.next().ok_or_else(|| bad(sub, "missing PORT"))?;
            if let Some(x) = a.next() {
                return Err(bad(sub, format!("unexpected `{x}`")));
            }
            Ok(Cmd::Who {
                port: port(sub, &p)?,
            })
        }
        _ => wait(&mut a),
    }
}

fn start(a: &mut Args) -> Result<Cmd, Failure> {
    let sub = a.sub;
    let n = name(sub, a.next_opt())?;
    let (mut port_arg, mut ready_arg, mut t) = (None, None, DEFAULT_TIMEOUT);
    let mut argv = Vec::new();
    while let Some(x) = a.next_opt() {
        if x == "--" {
            argv = a.rest();
            break;
        }
        if !x.starts_with('-') || x == "-" {
            argv = std::iter::once(x).chain(a.rest()).collect();
            break;
        }
        let (k, inline) = split_opt(&x);
        match k.as_str() {
            "--port" | "-p" => {
                let v = a.value(&k, inline)?;
                port_arg = Some(if v == "auto" || v == "0" {
                    PortArg::Auto
                } else {
                    PortArg::Fixed(port(sub, &v)?)
                });
            }
            "--ready" | "-r" => ready_arg = Some(ready(&a.value(&k, inline)?)?),
            "--timeout" | "-t" => t = timeout(sub, &a.value(&k, inline)?)?,
            _ => return Err(bad(sub, format!("unknown option `{x}`"))),
        }
    }
    if argv.is_empty() {
        return Err(bad(sub, "missing the command to run after `--`"));
    }
    if argv.len() == 1 && argv[0].contains(char::is_whitespace) {
        argv = vec!["sh".into(), "-c".into(), argv.remove(0)];
    }
    let mentions_port = argv.iter().any(|x| x.contains("{port}"));
    if mentions_port && port_arg.is_none() {
        return Err(
            bad(sub, "the command has {port} but no --port").hint("add --port auto (or --port N)")
        );
    }
    let ready = match ready_arg {
        Some(r @ (Ready::Http { .. } | Ready::Port)) if port_arg.is_none() => {
            let kind = if matches!(r, Ready::Port) {
                "port"
            } else {
                "http"
            };
            return Err(bad(sub, format!("--ready {kind} needs a port"))
                .hint("add --port auto and put {port} in the command, or --port N"));
        }
        Some(r) => r,
        None if port_arg.is_some() => Ready::Http { path: "/".into() },
        None => Ready::Settle,
    };
    Ok(Cmd::Start(StartArgs {
        name: n,
        port: port_arg,
        ready,
        timeout: t,
        argv,
    }))
}

fn logs(a: &mut Args) -> Result<Cmd, Failure> {
    let sub = a.sub;
    let n = name(sub, a.next())?;
    let (mut lines, mut follow, mut t) = (50usize, None, DEFAULT_TIMEOUT);
    while let Some(x) = a.next() {
        let (k, inline) = split_opt(&x);
        match k.as_str() {
            "-n" | "--lines" | "--tail" => {
                let v = a.value(&k, inline)?;
                lines = v
                    .parse()
                    .map_err(|_| bad(sub, format!("invalid -n `{v}`")))?;
            }
            "--follow-until" | "--until" | "-f" => {
                follow = Some(regex(sub, &a.value(&k, inline)?)?)
            }
            "--timeout" | "-t" => t = timeout(sub, &a.value(&k, inline)?)?,
            _ => return Err(bad(sub, format!("unexpected `{x}`"))),
        }
    }
    Ok(Cmd::Logs {
        name: n,
        lines,
        follow,
        timeout: t,
    })
}

fn url(text: &str) -> Result<WaitTarget, Failure> {
    if text.starts_with("https://") {
        return Err(bad(
            "wait",
            "https:// is not supported; wait for the port instead (wait port N)",
        ));
    }
    let invalid = || bad("wait", format!("invalid URL `{text}`"));
    let rest = text.strip_prefix("http://").unwrap_or(text);
    // The host ends at the path, the query or the fragment; the fragment is not sent.
    let (hostport, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let path = path.split('#').next().unwrap_or("");
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    // `[::1]:8080`: an IPv6 host keeps its brackets, as it goes in a URL and in `Host:`.
    let (host, p) = match hostport.strip_prefix('[') {
        Some(v6) => {
            let (ip, after) = v6.split_once(']').ok_or_else(invalid)?;
            if ip.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(invalid());
            }
            let p = match after {
                "" => 80,
                _ => port("wait", after.strip_prefix(':').ok_or_else(invalid)?)?,
            };
            (format!("[{ip}]"), p)
        }
        None => {
            let (h, p) = match hostport.rsplit_once(':') {
                Some((h, p)) => (h, port("wait", p)?),
                None => (hostport, 80),
            };
            if h.is_empty() || h.contains([']', ':', '@']) {
                return Err(invalid());
            }
            (h.to_string(), p)
        }
    };
    Ok(WaitTarget::Url {
        host,
        port: p,
        path,
        statuses: None,
    })
}

/// An option that `wait` reads (`--timeout 5`, `-t5`, `--status=200`, `--json`).
fn is_wait_option(arg: &str) -> bool {
    let (k, _) = split_opt(arg);
    [
        "--",
        "--timeout",
        "-t",
        "--interval",
        "--every",
        "-i",
        "--status",
        "-s",
        "--json",
        "-h",
        "--help",
    ]
    .contains(&k.as_str())
}

fn wait(a: &mut Args) -> Result<Cmd, Failure> {
    let sub = a.sub;
    let (mut target, mut t, mut interval) = (None, DEFAULT_TIMEOUT, None);
    let mut codes: Option<Statuses> = None;
    // After `cmd`: the first argument that is not an option starts the command.
    let mut cmd = false;
    while let Some(x) = a.next_opt() {
        if cmd && target.is_none() && !x.starts_with('-') && x.contains(char::is_whitespace) {
            // `cmd 'curl -sf URL' --timeout 5`: a quoted shell line is the whole command,
            // so what follows it can only be devsrv's.
            target = Some(WaitTarget::Cmd(vec!["sh".into(), "-c".into(), x]));
            continue;
        }
        if x == "--" || (cmd && target.is_none() && !x.starts_with('-')) {
            let mut argv = if x == "--" {
                a.rest()
            } else {
                std::iter::once(x).chain(a.rest()).collect()
            };
            if argv.is_empty() {
                return Err(bad(sub, "missing the command to run after `--`"));
            }
            if argv.len() == 1 && argv[0].contains(char::is_whitespace) {
                argv = vec!["sh".into(), "-c".into(), argv.remove(0)];
            }
            if target.replace(WaitTarget::Cmd(argv)).is_some() {
                return Err(bad(sub, "give one thing to wait for"));
            }
            break;
        }
        let (k, inline) = split_opt(&x);
        let set = |target: &mut Option<WaitTarget>, v: WaitTarget| {
            if target.replace(v).is_some() {
                Err(bad(sub, "give one thing to wait for"))
            } else {
                Ok(())
            }
        };
        match k.trim_start_matches("--") {
            "timeout" | "-t" => t = timeout(sub, &a.value(&k, inline)?)?,
            "interval" | "every" | "-i" => {
                let v = a.value(&k, inline)?;
                interval = Some(duration(&v).ok_or_else(|| {
                    bad(
                        sub,
                        format!("invalid --interval `{v}`: use seconds (1, 1s), 500ms or 2m"),
                    )
                })?)
            }
            "status" | "-s" => {
                let more = statuses(&a.value(&k, inline)?)?;
                codes.get_or_insert(Statuses(vec![])).0.extend(more.0);
            }
            "cmd" | "command" => {
                if target.is_some() {
                    return Err(bad(sub, "give one thing to wait for"));
                }
                if let Some(line) = inline {
                    // `--cmd='curl -sf x'`: one shell line.
                    a.items.insert(a.pos, line);
                }
                cmd = true;
            }
            "grep" => {
                return Err(bad(
                    sub,
                    "wait has no grep; a line of a file is `wait file PATH REGEX`",
                )
                .hint("devsrv wait file PATH REGEX (the path first, then the pattern)"));
            }
            "port" => {
                let v = a.value(&k, inline)?;
                set(&mut target, WaitTarget::Port(port(sub, &v)?))?
            }
            "file" => {
                let path = a.value(&k, inline)?.into();
                // An optional pattern: the next argument, unless it is an option of wait
                // (`200` and `-->` are patterns).
                let pattern = match a.peek() {
                    Some(p) if !is_wait_option(p) => {
                        let p = a.next().expect("peeked");
                        Some(regex(sub, &p)?)
                    }
                    _ => None,
                };
                set(&mut target, WaitTarget::File { path, pattern })?
            }
            "gone" => {
                let v = a.value(&k, inline)?;
                let pid = v
                    .parse()
                    .ok()
                    .filter(|&p: &i32| p > 0)
                    .ok_or_else(|| bad(sub, format!("invalid pid `{v}`")))?;
                set(&mut target, WaitTarget::Gone(pid))?
            }
            "log" => {
                let n = name(sub, inline.or_else(|| a.next()))?;
                let p = a
                    .next()
                    .ok_or_else(|| bad(sub, "wait log needs NAME and REGEX"))?;
                set(
                    &mut target,
                    WaitTarget::Log {
                        name: n,
                        pattern: regex(sub, &p)?,
                    },
                )?
            }
            "url" => set(&mut target, url(&a.value(&k, inline)?)?)?,
            _ if x.starts_with("http://") || x.starts_with("https://") => {
                set(&mut target, url(&x)?)?
            }
            _ if x.parse::<u16>().is_ok() => set(&mut target, WaitTarget::Port(port(sub, &x)?))?,
            _ => return Err(bad(sub, format!("unexpected `{x}`"))),
        }
    }
    if cmd && target.is_none() {
        return Err(bad(sub, "missing the command to run after `cmd --`"));
    }
    let mut target = target.ok_or_else(|| bad(sub, "nothing to wait for"))?;
    if let Some(c) = codes {
        match &mut target {
            WaitTarget::Url { statuses, .. } => *statuses = Some(c),
            _ => {
                return Err(bad(sub, "--status is for wait URL")
                    .hint("wait http://127.0.0.1:PORT/PATH --status 200"));
            }
        }
    }
    Ok(Cmd::Wait {
        target,
        timeout: t,
        interval,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, Failure> {
        parse(s.split(' ').map(String::from).collect()).1
    }

    #[test]
    fn start_forms() {
        let Ok(Cmd::Start(s)) = p(
            "start web --port=auto --ready http:health --timeout 2m -- python3 -m http.server {port}",
        ) else {
            panic!()
        };
        assert!(matches!(s.port, Some(PortArg::Auto)));
        assert_eq!(
            s.ready,
            Ready::Http {
                path: "/health".into()
            }
        );
        assert_eq!(s.timeout, Duration::from_secs(120));
        assert_eq!(s.argv, ["python3", "-m", "http.server", "{port}"]);
        let Ok(Cmd::Start(s)) = p("start w node a.js") else {
            panic!()
        };
        assert_eq!(s.ready, Ready::Settle);
        assert_eq!(s.argv, ["node", "a.js"]);
    }

    #[test]
    fn start_mistakes_are_usage_errors() {
        for bad in [
            "start web -- x {port}",
            "start web --ready http -- x",
            "start 1web -- x",
            "start web",
            "start web --ready nope -- x",
        ] {
            assert_eq!(p(bad).err().map(|f| f.code), Some(2), "{bad}");
        }
    }

    #[test]
    fn one_quoted_line_runs_in_sh() {
        let Ok(Cmd::Start(s)) = parse(vec![
            "start".into(),
            "w".into(),
            "--".into(),
            "npm run dev".into(),
        ])
        .1
        else {
            panic!()
        };
        assert_eq!(s.argv, ["sh", "-c", "npm run dev"]);
    }

    #[test]
    fn wait_forms() {
        assert!(matches!(
            p("wait http://127.0.0.1:8080/x"),
            Ok(Cmd::Wait {
                target: WaitTarget::Url { port: 8080, .. },
                ..
            })
        ));
        assert!(matches!(
            p("wait port 9 --timeout 1"),
            Ok(Cmd::Wait {
                target: WaitTarget::Port(9),
                ..
            })
        ));
        assert!(matches!(
            p("wait --port=9"),
            Ok(Cmd::Wait {
                target: WaitTarget::Port(9),
                ..
            })
        ));
        assert!(matches!(
            p("wait 9"),
            Ok(Cmd::Wait {
                target: WaitTarget::Port(9),
                ..
            })
        ));
        assert!(matches!(
            p("wait gone 42"),
            Ok(Cmd::Wait {
                target: WaitTarget::Gone(42),
                ..
            })
        ));
        assert!(matches!(
            p("wait log web ready"),
            Ok(Cmd::Wait {
                target: WaitTarget::Log { .. },
                ..
            })
        ));
        assert_eq!(p("wait https://x").err().map(|f| f.code), Some(2));
    }

    #[test]
    fn wait_urls() {
        let url = |s: &str| match p(&format!("wait {s}")) {
            Ok(Cmd::Wait {
                target: WaitTarget::Url {
                    host, port, path, ..
                },
                ..
            }) => (host, port, path),
            Ok(_) => panic!("{s}: not a URL"),
            Err(f) => panic!("{s}: {}", f.text()),
        };
        let want = |h: &str, port: u16, path: &str| (h.to_string(), port, path.to_string());
        assert_eq!(url("http://[::1]/"), want("[::1]", 80, "/"));
        assert_eq!(
            url("http://[::1]:8080/x?y=1"),
            want("[::1]", 8080, "/x?y=1")
        );
        assert_eq!(url("http://127.0.0.1:9?x=1"), want("127.0.0.1", 9, "/?x=1"));
        assert_eq!(
            url("http://localhost:3000/a#top"),
            want("localhost", 3000, "/a")
        );
        assert_eq!(url("http://h#top"), want("h", 80, "/"));
        for bad in [
            "http://[::1",
            "http://[zz]:1/",
            "http://[::1]x/",
            "http://:1/",
            "http://::1/",
        ] {
            assert_eq!(
                p(&format!("wait {bad}")).err().map(|f| f.code),
                Some(2),
                "{bad}"
            );
        }
    }

    #[test]
    fn wait_statuses() {
        let codes = |s: &str| match p(s) {
            Ok(Cmd::Wait {
                target: WaitTarget::Url { statuses, .. },
                ..
            }) => statuses.map(|s| s.0),
            _ => panic!("{s}"),
        };
        assert_eq!(codes("wait http://x:1/"), None);
        assert_eq!(
            codes("wait http://x:1/ --status 200"),
            Some(vec![(200, 200)])
        );
        assert_eq!(
            codes("wait --status=2xx,304 http://x:1/ -s 400-404"),
            Some(vec![(200, 299), (304, 304), (400, 404)])
        );
        for bad in [
            "2x0", "99", "600", "300-200", "", "200,", "1xx", "101", "100-299",
        ] {
            let s = format!("wait http://x:1/ --status {bad}");
            assert_eq!(p(&s).err().map(|f| f.code), Some(2), "{s}");
        }
        assert_eq!(p("wait port 9 --status 200").err().map(|f| f.code), Some(2));
    }

    #[test]
    fn wait_file_and_cmd_forms() {
        let Ok(Cmd::Wait {
            target: WaitTarget::File {
                pattern: Some(r), ..
            },
            timeout,
            ..
        }) = p("wait file out.log ready --timeout 5")
        else {
            panic!()
        };
        assert_eq!((r.as_str(), timeout), ("ready", Duration::from_secs(5)));
        assert!(matches!(
            p("wait --file out.log -t 5"),
            Ok(Cmd::Wait {
                target: WaitTarget::File { pattern: None, .. },
                ..
            })
        ));
        let argv = |s: Vec<&str>| match parse(s.into_iter().map(String::from).collect()).1 {
            Ok(Cmd::Wait {
                target: WaitTarget::Cmd(a),
                timeout,
                interval,
            }) => (a, timeout, interval),
            Ok(_) => panic!("not cmd"),
            Err(f) => panic!("{}", f.text()),
        };
        // After `--`, everything is the command's, --timeout too.
        let (a, t, _) = argv(vec![
            "wait",
            "cmd",
            "--",
            "curl",
            "-sf",
            "x",
            "--timeout",
            "5",
        ]);
        assert_eq!(a, ["curl", "-sf", "x", "--timeout", "5"]);
        assert_eq!(t, DEFAULT_TIMEOUT);
        let (a, t, i) = argv(vec![
            "wait",
            "-t",
            "5",
            "--interval",
            "2s",
            "cmd",
            "--",
            "true",
        ]);
        assert_eq!(
            (a, t, i),
            (
                vec!["true".to_string()],
                Duration::from_secs(5),
                Some(Duration::from_secs(2))
            )
        );
        let (a, _, _) = argv(vec!["wait", "cmd", "pg_isready", "-q"]);
        assert_eq!(a, ["pg_isready", "-q"]);
        // One quoted line is the whole command; options may follow it.
        let (a, t, _) = argv(vec![
            "wait",
            "cmd",
            "curl -sf x | grep ok",
            "--timeout",
            "5",
        ]);
        assert_eq!(a, ["sh", "-c", "curl -sf x | grep ok"]);
        assert_eq!(t, Duration::from_secs(5));
        let (a, _, _) = argv(vec!["wait", "--cmd=test -f x"]);
        assert_eq!(a, ["sh", "-c", "test -f x"]);
        for bad in [
            "wait cmd",
            "wait cmd --",
            "wait grep x y",
            "wait port 9 cmd -- true",
        ] {
            assert_eq!(p(bad).err().map(|f| f.code), Some(2), "{bad}");
        }
    }

    #[test]
    fn json_anywhere_before_dashdash() {
        let (json, cmd) = parse("ls --json".split(' ').map(String::from).collect());
        assert!(json && matches!(cmd, Ok(Cmd::Ls { all: false })));
        let (json, cmd) = parse(
            "start w -- echo --json"
                .split(' ')
                .map(String::from)
                .collect(),
        );
        let Ok(Cmd::Start(s)) = cmd else { panic!() };
        assert!(!json);
        assert_eq!(s.argv, ["echo", "--json"]);
    }

    #[test]
    fn durations() {
        assert_eq!(duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(duration("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(duration("0"), None);
    }
}
