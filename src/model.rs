//! Domain types. Values of these types are already validated: the CLI parser and the
//! record reader are the only places that build them from text.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::os::ExitStatus;

/// A process name: `[A-Za-z][A-Za-z0-9._-]{0,63}`. Starts with a letter, so it is never
/// taken for a pid, an option, or a path component such as `..`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Name(String);

impl Name {
    pub fn parse(text: &str) -> Option<Name> {
        let mut chars = text.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
        let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        (first_ok && rest_ok && text.len() <= 64).then(|| Name(text.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The namespace of names: the nearest directory at or above the working directory
/// that has a `.git` entry (so each worktree is its own scope), or the working
/// directory itself outside git. Always canonical. `git` is not run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(PathBuf);

impl Scope {
    pub fn of(cwd: &Path) -> std::io::Result<Scope> {
        let cwd = cwd.canonicalize()?;
        let top = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(&cwd);
        Ok(Scope(top.to_path_buf()))
    }
    pub fn from_recorded(path: PathBuf) -> Scope {
        Scope(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    /// Directory name under the state root: `<basename>-<FNV-1a 64 of the path>`.
    /// Hand-written so it stays the same across Rust versions.
    pub fn dir_name(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.0.as_os_str().as_encoded_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let base: String = self
            .0
            .file_name()
            .map(|n| n.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect())
            .unwrap_or_else(|| "root".into());
        format!("{base}-{h:016x}")
    }
}

/// Milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMs(pub u64);

impl UnixMs {
    pub fn now() -> UnixMs {
        UnixMs(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0))
    }
    pub fn elapsed(self) -> Duration {
        Duration::from_millis(UnixMs::now().0.saturating_sub(self.0))
    }
}

/// When a process started, as the OS reports it. Only compared for equality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StartTime(pub String);

/// A process as devsrv remembers it. A bare pid is never enough to signal anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcRef {
    pub pid: i32,
    pub started: StartTime,
}

impl ProcRef {
    /// The content of a `stop` file: which run the request is for.
    pub fn token(&self) -> String {
        format!("{} {}", self.pid, self.started.0)
    }
}

/// How `start` decides that the process is ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ready {
    /// `GET path` on the port gets a final response from 200 to 499.
    Http { path: String },
    /// The port accepts a TCP connection.
    Port,
    /// A line of the log (ANSI escapes removed) matches the pattern.
    Log { pattern: String },
    /// Nothing to probe: ready when the process still runs after 0.5 s.
    Settle,
}

/// What the supervisor runs, with nothing left to decide: the port is chosen, `{port}`
/// is substituted, `--ready` is filled in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    pub name: Name,
    pub scope: Scope,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub port: Option<u16>,
    pub ready: Ready,
    pub timeout_ms: u64,
    pub created: UnixMs,
}

impl Spec {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

pub const RECORD_VERSION: u32 = 1;

/// The record of one name, `<instance>/record.json`. Written only by the holder of the
/// name's lock, replaced atomically.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    pub spec: Spec,
    pub state: State,
}

/// What the record says. It never says "alive": that is derived from the lock.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The command was spawned. `ready: None` while the supervisor still waits.
    Up { supervisor: ProcRef, child: ProcRef, ready: Option<ReadyInfo> },
    /// Final. Holds nothing to signal.
    Down { ended: UnixMs, end: End },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadyInfo {
    pub waited_ms: u64,
    pub address: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// The command could not be executed.
    SpawnFailed { error: String, not_found: bool },
    Exited {
        pid: i32,
        status: ExitStatus,
        cause: Cause,
        /// Group members still alive after KILL.
        survivors: Vec<i32>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// Nobody asked: the command finished or crashed.
    OnItsOwn,
    /// Not ready within the timeout; the supervisor stopped it. `last` is what the last
    /// probe saw.
    StartTimedOut { last: String },
    /// The port answered, but another process (not in the group) listens on it.
    PortTaken { owners: Vec<i32> },
    /// `devsrv stop`. `forced`: KILL was needed.
    Stopped { forced: bool },
}

/// `1m05s`, `3.2s`, `2h10m`.
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(Name::parse("web").is_some());
        assert!(Name::parse("api-2.v1_x").is_some());
        for bad in ["", "1web", "-web", "..", "a/b", "a b", &"a".repeat(65)] {
            assert!(Name::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(human(Duration::from_millis(3200)), "3.2s");
        assert_eq!(human(Duration::from_secs(65)), "1m05s");
        assert_eq!(human(Duration::from_secs(7800)), "2h10m");
    }
}
