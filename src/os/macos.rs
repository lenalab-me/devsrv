use std::io;
use std::process::Command;
use std::time::Duration;

use super::{Lookup, ProcInfo};
use crate::model::StartTime;

pub fn siginfo_pid(info: &libc::siginfo_t) -> i32 {
    info.si_pid
}

pub fn proc_info(pid: i32) -> Lookup<ProcInfo> {
    // SAFETY: an all-zero proc_bsdinfo is valid; proc_pidinfo fills at most `size` bytes.
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bi).cast(), size) };
    if n != size {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => Lookup::Denied,
            _ => Lookup::Gone,
        };
    }
    Lookup::Found(ProcInfo {
        pid,
        pgid: bi.pbi_pgid as i32,
        started: StartTime(format!("{}.{:06}", bi.pbi_start_tvsec, bi.pbi_start_tvusec)),
        zombie: bi.pbi_status == libc::SZOMB,
    })
}

pub fn group_members(pgid: i32) -> io::Result<Vec<ProcInfo>> {
    let mut buf = vec![0 as libc::pid_t; 4096];
    let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // libproc returns the number of pids here (proc_listpids returns bytes;
    // proc_listpgrppids divides), and 0 both for an empty group and for a failure,
    // which only errno tells apart.
    // SAFETY: errno is thread-local; the buffer is `bytes` long.
    unsafe { *libc::__error() = 0 };
    let n = unsafe { libc::proc_listpgrppids(pgid, buf.as_mut_ptr().cast(), bytes) };
    let err = io::Error::last_os_error();
    if n < 0 || (n == 0 && err.raw_os_error().is_some_and(|e| e != 0)) {
        return Err(err);
    }
    let count = (n as usize).min(buf.len());
    let mut out = Vec::new();
    for &pid in buf.iter().take(count).filter(|&&p| p > 0) {
        match proc_info(pid) {
            Lookup::Found(p) if p.pgid == pgid => out.push(p),
            Lookup::Found(_) | Lookup::Gone => {}
            Lookup::Denied => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("pid {pid}"),
                ));
            }
        }
    }
    Ok(out)
}

pub fn command_line(pid: i32) -> Option<String> {
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-o", "command=", "-p", &pid.to_string()]);
    let out = super::run_helper(&mut cmd, Duration::from_secs(2)).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Pids listening on TCP `port`. `lsof` exits 1 when nothing matches.
pub fn tcp_listeners(port: u16) -> io::Result<Vec<i32>> {
    let mut cmd = Command::new("/usr/sbin/lsof");
    cmd.args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"]);
    let out = super::run_helper(&mut cmd, Duration::from_secs(3))?;
    let mut pids: Vec<i32> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.strip_prefix('p')?.parse().ok())
        .collect();
    pids.sort();
    pids.dedup();
    Ok(pids)
}
