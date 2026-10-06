//! `devsrv __supervise`: the owner of one name for the life of one run.
//!
//! Started detached by `devsrv start`, holding the name's lock (inherited as
//! `DEVSRV_LOCK_FD`), with the `Spec` on stdin. It spawns the command in its own process
//! group, writes `Up`, waits for readiness, then watches for the command's exit and for
//! a stop request. At the end it empties the group, reaps the command, writes `Down`,
//! and exits, which releases the lock. It never touches the command's output: the
//! command writes the log itself, so killing the supervisor leaves the command running.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex_lite::Regex;

use crate::model::{
    Cause, End, ProcRef, RECORD_VERSION, Ready, ReadyInfo, Record, Spec, StartTime, State, UnixMs,
};
use crate::os::{self, Child, Lookup};
use crate::probe::{self, Condition, Outcome};
use crate::store::Claim;

pub const LOCK_FD_ENV: &str = "DEVSRV_LOCK_FD";
pub const DIR_ENV: &str = "DEVSRV_INSTANCE_DIR";
const GRACE: Duration = Duration::from_secs(5);

enum Interrupt {
    Exited,
    Stop,
}

/// Closes every descriptor this process inherited except stdio and `keep`, so the
/// supervisor holds none of the caller's pipes (a held pipe keeps a tool call open).
fn close_inherited(keep: i32) {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let fds: Vec<i32> = fs::read_dir(dir)
        .map(|e| {
            e.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    for fd in fds.into_iter().filter(|&fd| fd > 2 && fd != keep) {
        // SAFETY: closing descriptors nothing in this fresh process uses yet.
        unsafe { libc::close(fd) };
    }
}

pub fn run() -> u8 {
    let lock_fd: i32 = match std::env::var(LOCK_FD_ENV).ok().and_then(|v| v.parse().ok()) {
        Some(fd) => fd,
        None => {
            eprintln!("devsrv __supervise is started by devsrv start, not by hand");
            return 2;
        }
    };
    let dir = PathBuf::from(std::env::var_os(DIR_ENV).unwrap_or_default());
    close_inherited(lock_fd);
    // SAFETY: devsrv start passed us this descriptor of the lock file, and only us.
    let claim = unsafe { Claim::inherit(lock_fd, dir) };
    let mut input = String::new();
    let spec: Spec = match std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_str(&input).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("devsrv: cannot read the spec: {e}");
            return 3;
        }
    };
    match supervise(&claim, &spec) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("devsrv: {e}");
            3
        }
    }
}

fn record(spec: &Spec, state: State) -> Record {
    Record {
        v: RECORD_VERSION,
        spec: spec.clone(),
        state,
    }
}

fn spawn(spec: &Spec, log: &PathBuf) -> std::io::Result<std::process::Child> {
    let out = OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .current_dir(&spec.cwd)
        .env_remove(LOCK_FD_ENV)
        .env_remove(DIR_ENV)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0);
    if let Some(p) = spec.port {
        cmd.env("PORT", p.to_string());
    }
    cmd.spawn()
}

fn readiness(spec: &Spec, log: PathBuf) -> Condition {
    match (&spec.ready, spec.port) {
        (Ready::Http { path }, Some(port)) => Condition::Http {
            host: None,
            port,
            path: path.clone(),
        },
        (Ready::Port, Some(port)) => Condition::Port { host: None, port },
        (Ready::Log { pattern }, _) => Condition::Log {
            path: log,
            pattern: Regex::new(pattern).expect("checked by the parser"),
        },
        _ => Condition::Delay(Instant::now() + Duration::from_millis(500)),
    }
}

/// The first line of `start`'s stdout.
fn address(spec: &Spec, child: i32, met_host: Option<&str>) -> String {
    match (&spec.ready, spec.port) {
        (Ready::Http { .. }, Some(p)) => format!("http://{}:{p}/", met_host.unwrap_or("127.0.0.1")),
        (_, Some(p)) => format!("{}:{p}", met_host.unwrap_or("127.0.0.1")),
        (_, None) => format!("pid {child}"),
    }
}

/// Listeners on the port that are not in the group. Empty when the group owns it, or
/// when the owners cannot be seen (then nothing is claimed either way).
fn foreign_listeners(port: u16, pgid: i32) -> Vec<i32> {
    let Ok(pids) = os::tcp_listeners(port) else {
        return vec![];
    };
    let ours = |pid: &i32| matches!(os::proc_info(*pid), Lookup::Found(p) if p.pgid == pgid);
    if pids.is_empty() || pids.iter().any(ours) {
        vec![]
    } else {
        pids
    }
}

fn supervise(claim: &Claim, spec: &Spec) -> Result<(), String> {
    let log = claim.dir().join("log");
    let stop = claim.dir().join("stop");
    let me = os::myself();
    let std_child = match spawn(spec, &log) {
        Ok(c) => c,
        Err(e) => {
            let end = End::SpawnFailed {
                error: e.to_string(),
                not_found: e.kind() == std::io::ErrorKind::NotFound,
            };
            return claim
                .write(&record(
                    spec,
                    State::Down {
                        ended: UnixMs::now(),
                        end,
                    },
                ))
                .map_err(|e| e.to_string());
        }
    };
    let child = Child::adopt(std_child);
    let started = match os::proc_info(child.pid()) {
        Lookup::Found(p) => p.started,
        _ => StartTime(String::new()),
    };
    let child_ref = ProcRef {
        pid: child.pid(),
        started,
    };
    let token = child_ref.token();
    let up = |ready: Option<ReadyInfo>| {
        record(
            spec,
            State::Up {
                supervisor: me.clone(),
                child: child_ref.clone(),
                ready,
            },
        )
    };
    let finish = |child: Child, cause: Cause| -> Result<(), String> {
        let t = child.terminate_group(GRACE);
        let cause = match cause {
            Cause::Stopped { .. } => Cause::Stopped { forced: t.forced },
            c => c,
        };
        let pid = child.pid();
        let status = child.reap();
        let _ = fs::remove_file(&stop);
        let end = End::Exited {
            pid,
            status,
            cause,
            survivors: t.survivors,
        };
        claim
            .write(&record(
                spec,
                State::Down {
                    ended: UnixMs::now(),
                    end,
                },
            ))
            .map_err(|e| e.to_string())
    };
    if let Err(e) = claim.write(&up(None)) {
        let _ = finish(child, Cause::OnItsOwn);
        return Err(format!("cannot write the record: {e}"));
    }
    let interrupted = || {
        if child.exited() {
            Some(Interrupt::Exited)
        } else if fs::read_to_string(&stop).is_ok_and(|t| t.trim() == token) {
            Some(Interrupt::Stop)
        } else {
            None
        }
    };
    match probe::wait_for(&readiness(spec, log.clone()), spec.timeout(), interrupted) {
        Outcome::Met(met, waited) => {
            if let Some(port) = spec
                .port
                .filter(|_| matches!(spec.ready, Ready::Http { .. } | Ready::Port))
            {
                let owners = foreign_listeners(port, child.pid());
                if !owners.is_empty() {
                    return finish(child, Cause::PortTaken { owners });
                }
            }
            let info = ReadyInfo {
                waited_ms: waited.as_millis() as u64,
                address: address(spec, child.pid(), met.host.as_deref()),
            };
            if let Err(e) = claim.write(&up(Some(info))) {
                let _ = finish(child, Cause::OnItsOwn);
                return Err(format!("cannot write the record: {e}"));
            }
        }
        Outcome::TimedOut(last) => return finish(child, Cause::StartTimedOut { last }),
        Outcome::Aborted(Interrupt::Exited) => return finish(child, Cause::OnItsOwn),
        Outcome::Aborted(Interrupt::Stop) => {
            return finish(child, Cause::Stopped { forced: false });
        }
    }
    loop {
        match interrupted() {
            Some(Interrupt::Exited) => return finish(child, Cause::OnItsOwn),
            Some(Interrupt::Stop) => return finish(child, Cause::Stopped { forced: false }),
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}
