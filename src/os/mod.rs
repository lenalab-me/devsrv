//! Everything that differs between macOS and Linux, and every syscall that can hurt.
//!
//! Invariant: `libc::kill` appears once in this crate, in `OwnedGroup::signal`, and its
//! target is always a process group that devsrv proved to be its own.

use std::io;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use crate::model::{ProcRef, StartTime};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as sys;

pub use sys::{command_line, tcp_listeners};

/// The answer to "what is this pid?". `Denied` (a sandbox, another user's process) is
/// neither "gone" nor "ours": nothing is signalled on it.
#[derive(Debug)]
pub enum Lookup<T> {
    Found(T),
    Gone,
    Denied,
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: i32,
    pub pgid: i32,
    pub started: StartTime,
    /// Exited but not reaped: holds its pid and stays in its group, but is not alive.
    pub zombie: bool,
}

pub fn proc_info(pid: i32) -> Lookup<ProcInfo> {
    if pid <= 0 {
        return Lookup::Gone;
    }
    sys::proc_info(pid)
}

/// The live (not zombie) members of a process group. An error means the answer is not
/// known, which is never taken as "empty".
pub fn group_members(pgid: i32) -> io::Result<Vec<ProcInfo>> {
    Ok(sys::group_members(pgid)?
        .into_iter()
        .filter(|p| !p.zombie)
        .collect())
}

/// Runs a helper (`ps`, `lsof`) with a time limit and returns its stdout, whatever its
/// exit status. An error when it cannot be run or does not finish in time. (Its own
/// `Child::kill` on a timeout targets this unreaped helper only.)
#[cfg(target_os = "macos")]
fn run_helper(cmd: &mut std::process::Command, limit: Duration) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let end = Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(reader.join().unwrap_or_default());
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "helper did not finish in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn myself() -> ProcRef {
    let pid = std::process::id() as i32;
    match proc_info(pid) {
        Lookup::Found(p) => ProcRef {
            pid,
            started: p.started,
        },
        _ => ProcRef {
            pid,
            started: StartTime(String::new()),
        },
    }
}

/// How a process ended, read when it is reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
    /// Nobody saw the exit (the supervisor had been killed before the command).
    Unknown,
}

/// A child this process spawned and has not reaped. While it is unreaped (even after it
/// exits, as a zombie) the kernel cannot give its pid, and so its pgid, to anyone else.
///
/// Built from `std::process::Child`, whose own `wait`/`try_wait` would reap it and must
/// not be used afterwards.
pub struct Child {
    pid: i32,
}

impl Child {
    pub fn adopt(child: std::process::Child) -> Child {
        Child {
            pid: child.id() as i32,
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Has the child exited? Does not reap it (`WNOWAIT`), so the pid stays pinned.
    pub fn exited(&self) -> bool {
        // SAFETY: an all-zero siginfo_t is valid; waitid only writes into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        // SAFETY: plain syscall on our own child with a valid out pointer.
        let r = unsafe { libc::waitid(libc::P_PID, self.pid as libc::id_t, &mut info, flags) };
        // With WNOHANG and no change, si_pid stays 0 (it was zeroed above).
        r == 0 && sys::siginfo_pid(&info) != 0
    }

    /// The child's process group. Borrowing the child keeps the proof alive: once the
    /// child is reaped (`reap` takes it by value) no `OwnedGroup` of it can exist.
    pub fn group(&self) -> OwnedGroup<'_> {
        OwnedGroup {
            pgid: self.pid,
            leader: None,
            _owner: PhantomData,
        }
    }

    /// Empties the child's group and waits for the child itself to exit, so that `reap`
    /// cannot block.
    pub fn terminate_group(&self, grace: Duration) -> Terminated {
        self.group().terminate(grace, || self.exited())
    }

    /// Reaps the child and reads how it ended. Blocks until it has exited.
    pub fn reap(self) -> ExitStatus {
        let mut status = 0;
        loop {
            // SAFETY: plain syscall on our own child.
            let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if r == self.pid {
                break;
            }
            if r == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return ExitStatus::Unknown;
        }
        if libc::WIFEXITED(status) {
            ExitStatus::Code(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            ExitStatus::Signal(libc::WTERMSIG(status))
        } else {
            ExitStatus::Unknown
        }
    }
}

/// What `terminate` did.
#[derive(Clone, Debug, Default)]
pub struct Terminated {
    /// The grace period ran out and KILL was sent.
    pub forced: bool,
    /// Members still alive after KILL.
    pub survivors: Vec<i32>,
    /// The group could not be read, so "nothing is left" is not known.
    pub unconfirmed: bool,
}

/// A process group devsrv has proved to be its own: the only thing in the crate that can
/// be signalled. Two ways to get one, and no other (the fields are private):
///
/// - `Child::group`: the leader is our unreaped child, so its pid, and the pgid, cannot
///   be reused. No time window.
/// - `OwnedGroup::verify`: the recorded leader still runs with the recorded start time.
///   Only for a name whose supervisor is gone. The proof is checked again right before
///   every signal (the leader still matches, or the group still has members, which
///   keeps the pgid from being reused); between that check and the signal the group
///   would have to empty and its id be reused.
pub struct OwnedGroup<'a> {
    pgid: i32,
    /// `Some` for a verified group: the leader to check again before each signal.
    leader: Option<ProcRef>,
    _owner: PhantomData<&'a Child>,
}

fn leader_matches(leader: &ProcRef) -> Lookup<()> {
    if leader.started.0.is_empty() {
        // The start time could not be read at spawn: nothing to compare with.
        return Lookup::Denied;
    }
    match proc_info(leader.pid) {
        Lookup::Found(p) if !p.zombie && p.pgid == leader.pid && p.started == leader.started => {
            Lookup::Found(())
        }
        Lookup::Found(_) | Lookup::Gone => Lookup::Gone,
        Lookup::Denied => Lookup::Denied,
    }
}

impl OwnedGroup<'static> {
    pub fn verify(leader: &ProcRef) -> Lookup<OwnedGroup<'static>> {
        match leader_matches(leader) {
            Lookup::Found(()) => Lookup::Found(OwnedGroup {
                pgid: leader.pid,
                leader: Some(leader.clone()),
                _owner: PhantomData,
            }),
            Lookup::Gone => Lookup::Gone,
            Lookup::Denied => Lookup::Denied,
        }
    }

    /// Stops a verified group. "Done" needs the leader gone too, as for a child.
    pub fn terminate_verified(&self, grace: Duration) -> Terminated {
        let leader = self.leader.clone().expect("built by verify");
        self.terminate(grace, || {
            !matches!(leader_matches(&leader), Lookup::Found(()))
        })
    }
}

impl OwnedGroup<'_> {
    fn may_signal(&self) -> bool {
        match &self.leader {
            None => true,
            // The leader still matches; or it has exited, its pid is not in use (so
            // the group id was not reused for a new group), and the group has members.
            Some(l) => match leader_matches(l) {
                Lookup::Found(()) => true,
                Lookup::Gone => {
                    matches!(proc_info(l.pid), Lookup::Gone)
                        && self.members().is_some_and(|m| !m.is_empty())
                }
                Lookup::Denied => false,
            },
        }
    }

    fn signal(&self, sig: libc::c_int) {
        if self.pgid <= 1 || !self.may_signal() {
            return;
        }
        // SAFETY: plain syscall. The target is a process group we proved to be ours.
        unsafe { libc::kill(-self.pgid, sig) };
    }

    /// Live members, or `None` when they cannot be read.
    pub fn members(&self) -> Option<Vec<i32>> {
        group_members(self.pgid)
            .ok()
            .map(|v| v.into_iter().map(|p| p.pid).collect())
    }

    /// TERM, wait up to `grace` for the group to empty and `leader_gone`, KILL, wait up
    /// to 2 s. TERM is sent even when the group looks empty: a listing can miss a member,
    /// and a signal to an empty group of ours reaches nobody.
    fn terminate(&self, grace: Duration, leader_gone: impl Fn() -> bool) -> Terminated {
        self.signal(libc::SIGTERM);
        if self.wait_gone(grace, &leader_gone) {
            return Terminated::default();
        }
        self.signal(libc::SIGKILL);
        if self.wait_gone(Duration::from_secs(2), &leader_gone) {
            return Terminated {
                forced: true,
                ..Terminated::default()
            };
        }
        match self.members() {
            Some(m) if !m.is_empty() => Terminated {
                forced: true,
                survivors: m,
                unconfirmed: false,
            },
            _ => Terminated {
                forced: true,
                survivors: vec![],
                unconfirmed: true,
            },
        }
    }

    fn wait_gone(&self, limit: Duration, leader_gone: &impl Fn() -> bool) -> bool {
        let end = Instant::now() + limit;
        loop {
            if self.members().is_some_and(|m| m.is_empty()) && leader_gone() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn spawn_tree(script: &str) -> Child {
        let c = std::process::Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap();
        Child::adopt(c)
    }

    fn wait_members(pgid: i32, n: usize) -> Vec<ProcInfo> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let m = group_members(pgid).unwrap();
            if m.len() == n || Instant::now() > end {
                return m;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_group_is_listed_and_emptied_with_its_grandchildren() {
        let child = spawn_tree("sleep 300 & sleep 301 & wait");
        let pgid = child.pid();
        assert_eq!(wait_members(pgid, 3).len(), 3, "sh and two sleeps");
        let t = child.terminate_group(Duration::from_secs(5));
        assert!(t.survivors.is_empty());
        assert!(group_members(pgid).unwrap().is_empty());
        assert_eq!(child.reap(), ExitStatus::Signal(libc::SIGTERM));
    }

    #[test]
    fn a_member_that_ignores_term_gets_kill() {
        let child = spawn_tree("trap '' TERM; sleep 300 & wait");
        wait_members(child.pid(), 2);
        let t = child.terminate_group(Duration::from_millis(300));
        assert!(t.forced && t.survivors.is_empty());
        assert_eq!(child.reap(), ExitStatus::Signal(libc::SIGKILL));
    }

    #[test]
    fn exit_is_seen_without_reaping_and_the_pid_stays_pinned() {
        let child = spawn_tree("exit 3");
        let end = Instant::now() + Duration::from_secs(5);
        while !child.exited() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(child.exited());
        // macOS answers Gone for a zombie, Linux Found with the zombie mark.
        assert!(
            !matches!(proc_info(child.pid()), Lookup::Found(p) if !p.zombie),
            "not alive"
        );
        assert!(child.exited(), "WNOWAIT leaves it for later");
        assert_eq!(child.reap(), ExitStatus::Code(3));
    }

    #[test]
    fn verify_refuses_a_different_start_time() {
        let child = spawn_tree("sleep 300");
        let info = match proc_info(child.pid()) {
            Lookup::Found(p) => p,
            other => panic!("{other:?}"),
        };
        let wrong = ProcRef {
            pid: child.pid(),
            started: StartTime("1.000000".into()),
        };
        assert!(matches!(OwnedGroup::verify(&wrong), Lookup::Gone));
        let right = ProcRef {
            pid: child.pid(),
            started: info.started,
        };
        assert!(matches!(OwnedGroup::verify(&right), Lookup::Found(_)));
        child.terminate_group(Duration::from_secs(2));
        child.reap();
    }
}
