use std::collections::HashSet;
use std::fs;
use std::io;

use super::{Lookup, ProcInfo};
use crate::model::StartTime;

pub fn siginfo_pid(info: &libc::siginfo_t) -> i32 {
    // SAFETY: waitid filled in a SIGCHLD siginfo, or left it zeroed.
    unsafe { info.si_pid() }
}

fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Fields of `/proc/<pid>/stat` after the command name, which may hold spaces and `)`.
fn stat_fields(text: &str) -> Option<Vec<&str>> {
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().collect())
}

fn parse_stat(pid: i32, text: &str, boot: &str) -> Option<ProcInfo> {
    let f = stat_fields(text)?;
    // proc_pid_stat(5): field 3 is the state, 5 the pgrp, 22 the start time.
    Some(ProcInfo {
        pid,
        pgid: f.get(2)?.parse().ok()?,
        started: StartTime(format!("{boot}:{}", f.get(19)?)),
        zombie: *f.first()? == "Z",
    })
}

pub fn proc_info(pid: i32) -> Lookup<ProcInfo> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => match parse_stat(pid, &text, &boot_id()) {
            Some(p) => Lookup::Found(p),
            None => Lookup::Denied,
        },
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Lookup::Denied,
        Err(_) => Lookup::Gone,
    }
}

fn all_pids() -> io::Result<Vec<i32>> {
    Ok(fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect())
}

pub fn group_members(pgid: i32) -> io::Result<Vec<ProcInfo>> {
    let boot = boot_id();
    let mut out = Vec::new();
    // A process that cannot be read is not taken as absent: the answer would be wrong.
    for pid in all_pids()? {
        let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let p = parse_stat(pid, &text, &boot)
            .ok_or_else(|| io::Error::other(format!("cannot parse /proc/{pid}/stat")))?;
        if p.pgid == pgid {
            out.push(p);
        }
    }
    Ok(out)
}

pub fn command_line(pid: i32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let s = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a))
        .collect::<Vec<_>>()
        .join(" ");
    (!s.is_empty()).then_some(s)
}

/// Socket inodes listening on `port`, from `/proc/net/tcp` and `tcp6`.
fn listening_inodes(port: u16) -> io::Result<HashSet<u64>> {
    let mut inodes = HashSet::new();
    // tcp6 is missing when IPv6 is off; tcp must be readable.
    for (file, required) in [("/proc/net/tcp", true), ("/proc/net/tcp6", false)] {
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) if required => return Err(e),
            Err(_) => continue,
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // local_address is HEXADDR:HEXPORT; state 0A is LISTEN.
            let Some(local) = f.get(1) else { continue };
            let Some((_, p)) = local.rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(p, 16).ok() == Some(port)
                && f.get(3) == Some(&"0A")
                && let Some(inode) = f.get(9).and_then(|s| s.parse().ok())
            {
                inodes.insert(inode);
            }
        }
    }
    Ok(inodes)
}

pub fn tcp_listeners(port: u16) -> io::Result<Vec<i32>> {
    let inodes = listening_inodes(port)?;
    if inodes.is_empty() {
        return Ok(vec![]);
    }
    let mut pids = Vec::new();
    for pid in all_pids()? {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let owns = fds
            .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
            .any(|link| {
                link.to_str()
                    .and_then(|s| {
                        s.strip_prefix("socket:[")?
                            .strip_suffix(']')?
                            .parse::<u64>()
                            .ok()
                    })
                    .is_some_and(|i| inodes.contains(&i))
            });
        if owns {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the listener's owner is not visible",
        ));
    }
    Ok(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_spaces_and_parens_in_the_name() {
        let text = "42 (a b) c)) S 1 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 98765 1000 1";
        let p = parse_stat(42, text, "boot").unwrap();
        assert_eq!(p.pgid, 42);
        assert_eq!(p.started, StartTime("boot:98765".into()));
        assert!(!p.zombie);
    }
}
