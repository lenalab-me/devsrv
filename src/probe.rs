//! Conditions to wait for, the one loop that waits for them, and reading logs.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex_lite::Regex;

use crate::os::{ExitStatus, Foreground, Lookup, proc_info};

/// The HTTP statuses that count as ready: ranges, both ends included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statuses(pub Vec<(u16, u16)>);

impl Statuses {
    /// What `--ready http` and a plain `wait URL` accept: the server answers, even with
    /// "not found", but is not failing.
    pub fn any_answer() -> Statuses {
        Statuses(vec![(200, 499)])
    }
    pub fn contains(&self, code: u16) -> bool {
        self.0.iter().any(|&(lo, hi)| (lo..=hi).contains(&code))
    }
}

impl std::fmt::Display for Statuses {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|&(lo, hi)| {
                if lo == hi {
                    lo.to_string()
                } else {
                    format!("{lo}-{hi}")
                }
            })
            .collect();
        f.write_str(&parts.join(","))
    }
}

pub enum Condition {
    /// A final HTTP response with one of `statuses` on `host:port` (or, with no host, on
    /// 127.0.0.1 then ::1).
    Http {
        host: Option<String>,
        port: u16,
        path: String,
        statuses: Statuses,
    },
    Port {
        host: Option<String>,
        port: u16,
    },
    /// A line of the file matches. Made with `Condition::log`.
    Log {
        path: PathBuf,
        pattern: Regex,
        scan: RefCell<Scan>,
    },
    File(PathBuf),
    /// The command exits 0. The process group of each run is killed when the run ends,
    /// and at the deadline.
    Cmd(Vec<String>),
    /// The process is gone: no longer there, a zombie, or its pid was reused.
    Gone {
        pid: i32,
        started: Option<crate::model::StartTime>,
    },
    /// The moment has come (`--ready none`: still running after a short while).
    Delay(Instant),
}

impl Condition {
    pub fn log(path: PathBuf, pattern: Regex) -> Condition {
        Condition::Log {
            path,
            pattern,
            scan: RefCell::default(),
        }
    }
}

/// How far the lines of a file have been read: none before `offset` matched, and
/// `partial` is the start of a line not yet ended. Each check reads only what was
/// written since the last one.
#[derive(Default)]
pub struct Scan {
    /// The file read (device and inode): another one is read from its start.
    file: Option<(u64, u64)>,
    offset: u64,
    partial: Vec<u8>,
}

/// What one read of the file takes at most; the check looks at the clock between reads.
const CHUNK: usize = 1 << 20;

/// A line, ANSI escapes and the `\r` of `\r\n` removed, if it matches.
fn matching(line: &[u8], pattern: &Regex) -> Option<String> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    let text = if text.contains('\x1b') {
        strip_ansi(&text)
    } else {
        text.into_owned()
    };
    pattern.is_match(&text).then_some(text)
}

/// The first line written since the last call that matches. The last line counts even
/// without a final newline. Reading stops at the deadline, and goes on from there next
/// time; a file that was truncated or replaced is read again from its start.
fn scan_lines(
    path: &Path,
    pattern: &Regex,
    scan: &mut Scan,
    deadline: Instant,
) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("no such file".into()),
        Err(e) => return Err(format!("cannot read it: {e}")),
    };
    let meta = f.metadata().map_err(|e| format!("cannot read it: {e}"))?;
    let id = (meta.dev(), meta.ino());
    if scan.file != Some(id) || meta.len() < scan.offset {
        *scan = Scan {
            file: Some(id),
            ..Scan::default()
        };
    }
    f.seek(SeekFrom::Start(scan.offset))
        .map_err(|e| format!("cannot read it: {e}"))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read it: {e}")),
        };
        scan.offset += n as u64;
        let mut rest = &buf[..n];
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            let found = if scan.partial.is_empty() {
                matching(&rest[..i], pattern)
            } else {
                scan.partial.extend_from_slice(&rest[..i]);
                let found = matching(&scan.partial, pattern);
                scan.partial.clear();
                found
            };
            if let Some(line) = found {
                return Ok(line);
            }
            rest = &rest[i + 1..];
        }
        scan.partial.extend_from_slice(rest);
        // A line this long is looked at once, and its end read as a line of its own.
        if scan.partial.len() > CHUNK {
            if let Some(line) = matching(&scan.partial, pattern) {
                return Ok(line);
            }
            scan.partial.clear();
        }
        if Instant::now() >= deadline {
            return Err("no line matched in the part of the file read in time".into());
        }
    }
    if !scan.partial.is_empty()
        && let Some(line) = matching(&scan.partial, pattern)
    {
        return Ok(line);
    }
    Err("no line matched".into())
}

/// What a met condition saw.
pub struct Met {
    /// `http://127.0.0.1:N/path`, `127.0.0.1:N`, the matched log line, the path...
    pub what: String,
    /// For `Http` and `Port`: the host that answered, as it goes in a URL.
    pub host: Option<String>,
}

impl Met {
    fn new(what: String) -> Met {
        Met { what, host: None }
    }
}

pub enum Outcome<A> {
    Met(Met, Duration),
    /// The last thing a probe saw ("connection refused", "HTTP 503", "no line matched").
    TimedOut(String),
    Aborted(A),
}

/// The pause between probes, unless the caller asks for another.
pub const EVERY: Duration = Duration::from_millis(50);
/// The pause between runs of `Condition::Cmd`: a command costs more than a connect.
pub const CMD_EVERY: Duration = Duration::from_millis(500);

/// Probes until the condition holds, the deadline passes, or `abort` returns something.
/// The first probe is immediate; then one every 50 ms. A single probe never runs past
/// the deadline, and none starts at it: one with no time left would only say that, in
/// place of what the last one saw.
pub fn wait_for<A>(
    cond: &Condition,
    limit: Duration,
    abort: impl FnMut() -> Option<A>,
) -> Outcome<A> {
    wait_every(cond, limit, EVERY, abort)
}

/// `wait_for` with another pause between probes.
pub fn wait_every<A>(
    cond: &Condition,
    limit: Duration,
    every: Duration,
    mut abort: impl FnMut() -> Option<A>,
) -> Outcome<A> {
    let start = Instant::now();
    let deadline = start + limit;
    let mut last: Option<String> = None;
    loop {
        last = match check(cond, deadline) {
            Ok(met) => return Outcome::Met(met, start.elapsed()),
            // A run the deadline cut short says less than the last one that finished.
            Err(seen) if seen.starts_with(CUT) && last.is_some() => last,
            Err(seen) => Some(seen),
        };
        if let Some(a) = abort() {
            return Outcome::Aborted(a);
        }
        let now = Instant::now();
        if now >= deadline {
            return Outcome::TimedOut(last.unwrap_or_default());
        }
        std::thread::sleep(every.min(deadline - now));
        if Instant::now() >= deadline {
            return Outcome::TimedOut(last.unwrap_or_default());
        }
    }
}

fn addrs(host: &Option<String>, port: u16) -> Vec<SocketAddr> {
    match host {
        Some(h) => {
            use std::net::ToSocketAddrs;
            // `[::1]` is written with brackets; the resolver takes the address alone.
            let h = h.trim_start_matches('[').trim_end_matches(']');
            (h, port)
                .to_socket_addrs()
                .map(|a| a.collect())
                .unwrap_or_default()
        }
        None => vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ],
    }
}

fn connect(addr: &SocketAddr, deadline: Instant) -> Result<TcpStream, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    let t = left
        .min(Duration::from_millis(250))
        .max(Duration::from_millis(1));
    TcpStream::connect_timeout(addr, t).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            "connection timed out".to_string()
        }
        _ => e.to_string(),
    })
}

fn url_host(addr: &SocketAddr, host: &Option<String>) -> String {
    match (host, addr.ip()) {
        (Some(h), _) => h.clone(),
        (None, IpAddr::V4(ip)) => ip.to_string(),
        (None, IpAddr::V6(ip)) => format!("[{ip}]"),
    }
}

/// The first address that answers, in order. When a later one answers (::1 after
/// 127.0.0.1 did not), the first is tried once more: a server that was still binding
/// both would otherwise get a different URL from run to run.
fn first_answer<T>(
    addrs: &[SocketAddr],
    try_one: impl Fn(&SocketAddr) -> Result<(String, T), String>,
) -> Result<(String, T), String> {
    let mut seen = String::from("no address");
    for (i, addr) in addrs.iter().enumerate() {
        match try_one(addr) {
            Ok(found) if i > 0 => return Ok(try_one(&addrs[0]).unwrap_or(found)),
            Ok(found) => return Ok(found),
            Err(e) => seen = e,
        }
    }
    Err(seen)
}

/// The host, as it goes in a URL, that accepts a connection on the port: 127.0.0.1, then
/// ::1. For a readiness that did not connect (a log line), to say where the server is.
pub fn answering_host(port: u16) -> Option<String> {
    let cond = Condition::Port { host: None, port };
    check(&cond, Instant::now() + Duration::from_millis(500))
        .ok()?
        .host
}

/// One HTTP/1.0 GET; the status code, or what went wrong.
fn http_status(
    addr: &SocketAddr,
    host: &str,
    path: &str,
    deadline: Instant,
) -> Result<u16, String> {
    let mut s = connect(addr, deadline)?;
    // One probe gets at most 2 s, and never runs past the caller's deadline.
    let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
    let left = || deadline.saturating_duration_since(Instant::now());
    let _ = s.set_write_timeout(Some(left().max(Duration::from_millis(1))));
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}:{}\r\nUser-Agent: devsrv\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes())
        .map_err(|e| format!("could not send the request: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Skip interim 1xx responses: the first final status line counts.
    loop {
        if left().is_zero() {
            return Err("connected, but no complete HTTP status line in time".into());
        }
        let _ = s.set_read_timeout(Some(left()));
        let n = s
            .read(&mut chunk)
            .map_err(|_| "connected, but no HTTP response".to_string())?;
        if n == 0 {
            return Err("connected, but the connection closed without an HTTP response".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        let codes: Vec<u16> = text
            .split("\r\n")
            .filter(|l| l.starts_with("HTTP/"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse().ok())
            .collect();
        if let Some(&code) = codes.iter().find(|&&c| c >= 200) {
            return Ok(code);
        }
        if buf.len() > 16 * 1024 {
            return Err("connected, but the reply is not HTTP".into());
        }
    }
}

fn check(cond: &Condition, deadline: Instant) -> Result<Met, String> {
    match cond {
        Condition::Http {
            host,
            port,
            path,
            statuses,
        } => {
            let try_one = |addr: &SocketAddr| {
                let h = url_host(addr, host);
                match http_status(addr, &h, path, deadline) {
                    Ok(code) if statuses.contains(code) => Ok((h, code)),
                    Ok(code) => Err(format!("HTTP {code}")),
                    Err(e) => Err(e),
                }
            };
            let (h, code) = first_answer(&addrs(host, *port), try_one)?;
            Ok(Met {
                what: format!("http://{h}:{port}{path} (HTTP {code})"),
                host: Some(h),
            })
        }
        Condition::Port { host, port } => {
            let try_one =
                |addr: &SocketAddr| connect(addr, deadline).map(|_| (url_host(addr, host), ()));
            let (h, ()) = first_answer(&addrs(host, *port), try_one)?;
            Ok(Met {
                what: format!("{h}:{port}"),
                host: Some(h),
            })
        }
        Condition::Log {
            path,
            pattern,
            scan,
        } => scan_lines(path, pattern, &mut scan.borrow_mut(), deadline).map(Met::new),
        Condition::Cmd(argv) => run_once(argv, deadline),
        Condition::File(p) => {
            if p.is_file() {
                Ok(Met::new(p.display().to_string()))
            } else {
                Err("no such file".into())
            }
        }
        Condition::Gone { pid, started } => match proc_info(*pid) {
            Lookup::Gone => Ok(Met::new(format!("pid {pid} is gone"))),
            Lookup::Found(p) if p.zombie || started.as_ref().is_some_and(|s| *s != p.started) => {
                Ok(Met::new(format!("pid {pid} is gone")))
            }
            Lookup::Found(_) => Err(format!("pid {pid} is still running")),
            Lookup::Denied => Err(format!("pid {pid} cannot be observed")),
        },
        Condition::Delay(at) => {
            if Instant::now() >= *at {
                Ok(Met::new("still running".into()))
            } else {
                Err("still settling".into())
            }
        }
    }
}

/// How many lines of a failed run's output the failure shows.
const CMD_TAIL: usize = 10;

/// What a run of `Condition::Cmd` that was still going at the deadline saw.
pub const CUT: &str = "still running at the deadline; killed it";

/// `curl -sf 'http://x/a b'`: the command as one would type it.
pub fn shown(argv: &[String]) -> String {
    if let [sh, c, line] = argv
        && sh == "sh"
        && c == "-c"
    {
        return line.clone();
    }
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.contains(|c: char| c.is_whitespace() || "'\"$`\\".contains(c)) {
                format!("'{}'", a.replace('\'', r"'\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One run of a check command. Its output goes to an unlinked file, not a pipe, so a
/// child it leaves behind cannot keep the read open. When the run ends, or at the
/// deadline, its process group is killed while the leader is still unreaped, so the group
/// id cannot have been reused; nothing of one run is left for the next.
/// The error is "exit 1" (or so), then the last lines of the output, one per line.
fn run_once(argv: &[String], deadline: Instant) -> Result<Met, String> {
    let mut out = scratch().map_err(|e| format!("cannot make a file for the output: {e}"))?;
    let (o, e) = match (out.try_clone(), out.try_clone()) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(x), _) | (_, Err(x)) => return Err(format!("cannot make a file for the output: {x}")),
    };
    let run = Foreground::spawn(
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(o)
            .stderr(e),
    )
    .map_err(|e| format!("cannot run it: {e}"))?;
    let cut = loop {
        if run.exited() {
            break false;
        }
        let now = Instant::now();
        if now >= deadline {
            break true;
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline - now));
    };
    let mut text = match run.finish() {
        _ if cut => CUT.into(),
        ExitStatus::Code(0) => return Ok(Met::new(format!("`{}` exited 0", shown(argv)))),
        ExitStatus::Code(c) => format!("exit {c}"),
        ExitStatus::Signal(n) => format!("killed by signal {n}"),
        ExitStatus::Unknown => "ended".into(),
    };
    let (lines, _) = tail_of(&mut out, CMD_TAIL);
    for l in lines {
        text.push('\n');
        text.push_str(&l);
    }
    Err(text)
}

/// A file for one run's output, already unlinked: it goes away with the last handle.
fn scratch() -> std::io::Result<fs::File> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "devsrv-wait-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _ = fs::remove_file(&path);
    Ok(f)
}

/// Removes ANSI escape sequences (colors, cursor moves) so patterns see plain text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The last `n` lines of a file, reading at most the last 1 MiB, and the total count
/// of lines in what was read.
pub fn tail(path: &Path, n: usize) -> (Vec<String>, usize) {
    let Ok(mut f) = fs::File::open(path) else {
        return (vec![], 0);
    };
    tail_of(&mut f, n)
}

fn tail_of(f: &mut fs::File, n: usize) -> (Vec<String>, usize) {
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(1 << 20);
    let _ = f.seek(SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let total = lines.len();
    (
        lines[total.saturating_sub(n)..]
            .iter()
            .map(|l| l.to_string())
            .collect(),
        total,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_removed() {
        assert_eq!(
            strip_ansi("\x1b[32m  ➜  Local:\x1b[0m http://x"),
            "  ➜  Local: http://x"
        );
    }

    #[test]
    fn tail_counts_a_last_line_without_newline() {
        let dir = std::env::temp_dir().join(format!("devsrv-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log");
        fs::write(&p, "1\n2\n3\n4\n5\n6\n7").unwrap();
        assert_eq!(
            tail(&p, 5),
            (
                vec!["3", "4", "5", "6", "7"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                7
            )
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
