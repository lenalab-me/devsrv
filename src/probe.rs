//! Conditions to wait for, the one loop that waits for them, and reading logs.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex_lite::Regex;

use crate::os::{Lookup, proc_info};

pub enum Condition {
    /// A final HTTP response from 200 to 499 on `host:port` (or, with no host, on
    /// 127.0.0.1 then ::1).
    Http { host: Option<String>, port: u16, path: String },
    Port { host: Option<String>, port: u16 },
    Log { path: PathBuf, pattern: Regex },
    File(PathBuf),
    /// The process is gone: no longer there, a zombie, or its pid was reused.
    Gone { pid: i32, started: Option<crate::model::StartTime> },
    /// The moment has come (`--ready none`: still running after a short while).
    Delay(Instant),
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

/// Probes until the condition holds, the deadline passes, or `abort` returns something.
/// The first probe is immediate; then one every 50 ms. A single probe never runs past
/// the deadline.
pub fn wait_for<A>(cond: &Condition, limit: Duration, mut abort: impl FnMut() -> Option<A>) -> Outcome<A> {
    let start = Instant::now();
    let deadline = start + limit;
    loop {
        let last = match check(cond, deadline) {
            Ok(met) => return Outcome::Met(met, start.elapsed()),
            Err(seen) => seen,
        };
        if let Some(a) = abort() {
            return Outcome::Aborted(a);
        }
        let now = Instant::now();
        if now >= deadline {
            return Outcome::TimedOut(last);
        }
        std::thread::sleep(Duration::from_millis(50).min(deadline - now));
    }
}

fn addrs(host: &Option<String>, port: u16) -> Vec<SocketAddr> {
    match host {
        Some(h) => {
            use std::net::ToSocketAddrs;
            (h.as_str(), port).to_socket_addrs().map(|a| a.collect()).unwrap_or_default()
        }
        None => vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ],
    }
}

fn connect(addr: &SocketAddr, deadline: Instant) -> Result<TcpStream, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    let t = left.min(Duration::from_millis(250)).max(Duration::from_millis(1));
    TcpStream::connect_timeout(addr, t).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "connection timed out".to_string(),
        _ => e.to_string(),
    })
}

fn url_host(addr: &SocketAddr, host: &Option<String>) -> String {
    match (host, addr.ip()) {
        (Some(h), _) => h.clone(),
        (None, IpAddr::V4(ip)) => ip.to_string(),
        (None, IpAddr::V6(_)) => "localhost".to_string(),
    }
}

/// One HTTP/1.0 GET; the status code, or what went wrong.
fn http_status(addr: &SocketAddr, host: &str, path: &str, deadline: Instant) -> Result<u16, String> {
    let mut s = connect(addr, deadline)?;
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(50));
    let _ = s.set_read_timeout(Some(left.min(Duration::from_secs(2))));
    let _ = s.set_write_timeout(Some(left.min(Duration::from_secs(2))));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{}\r\nUser-Agent: devsrv\r\nConnection: close\r\n\r\n", addr.port());
    s.write_all(req.as_bytes()).map_err(|e| format!("could not send the request: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Skip interim 1xx responses: the first final status line counts.
    loop {
        let n = s.read(&mut chunk).map_err(|_| "connected, but no HTTP response".to_string())?;
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
        Condition::Http { host, port, path } => {
            let mut seen = String::from("no address");
            for addr in addrs(host, *port) {
                let h = url_host(&addr, host);
                match http_status(&addr, &h, path, deadline) {
                    Ok(code) if (200..500).contains(&code) => {
                        return Ok(Met { what: format!("http://{h}:{port}{path} (HTTP {code})"), host: Some(h) });
                    }
                    Ok(code) => seen = format!("HTTP {code}"),
                    Err(e) => seen = e,
                }
            }
            Err(seen)
        }
        Condition::Port { host, port } => {
            let mut seen = String::from("no address");
            for addr in addrs(host, *port) {
                match connect(&addr, deadline) {
                    Ok(_) => {
                        let h = url_host(&addr, host);
                        return Ok(Met { what: format!("{h}:{port}"), host: Some(h) });
                    }
                    Err(e) => seen = e,
                }
            }
            Err(seen)
        }
        Condition::Log { path, pattern } => match find_line(path, pattern) {
            Some(line) => Ok(Met::new(line)),
            None => Err("no line matched".into()),
        },
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

/// The first line of the log that matches, ANSI escapes removed. The last line counts
/// even without a final newline.
pub fn find_line(path: &Path, pattern: &Regex) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(strip_ansi)
        .find(|l| pattern.is_match(l))
}

/// The last `n` lines of a file, reading at most the last 1 MiB, and the total count
/// of lines in what was read.
pub fn tail(path: &Path, n: usize) -> (Vec<String>, usize) {
    let Ok(mut f) = fs::File::open(path) else { return (vec![], 0) };
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
    (lines[total.saturating_sub(n)..].iter().map(|l| l.to_string()).collect(), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_removed() {
        assert_eq!(strip_ansi("\x1b[32m  ➜  Local:\x1b[0m http://x"), "  ➜  Local: http://x");
    }

    #[test]
    fn tail_counts_a_last_line_without_newline() {
        let dir = std::env::temp_dir().join(format!("devsrv-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("log");
        fs::write(&p, "1\n2\n3\n4\n5\n6\n7").unwrap();
        assert_eq!(tail(&p, 5), (vec!["3", "4", "5", "6", "7"].into_iter().map(String::from).collect(), 7));
        fs::remove_dir_all(&dir).unwrap();
    }
}
