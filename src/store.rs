//! The state directory: where records, logs and locks live, who may write them, and what
//! a reader sees.
//!
//! ```text
//! <root>/<project>-<hash>/root          the project path; a mismatch is refused
//! <root>/<project>-<hash>/<name>/lock   flock(LOCK_EX) held by the supervisor for its whole life
//! <root>/<project>-<hash>/<name>/record.json
//! <root>/<project>-<hash>/<name>/log
//! <root>/<project>-<hash>/<name>/stop   a stop request: the token of the run it is for
//! <root>/<project>-<hash>/<name>/supervisor.err
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{Name, RECORD_VERSION, Record, Scope, State};
use crate::report::Failure;

pub struct Store {
    root: PathBuf,
}

fn mkdir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

fn unwritable(path: &Path, e: &io::Error) -> Failure {
    Failure::system(format!("cannot write the state directory {}: {e}", path.display()))
        .hint("point devsrv at a writable directory and keep it for later calls: export DEVSRV_STATE_DIR=\"$PWD/.devsrv-state\"")
}

impl Store {
    /// `DEVSRV_STATE_DIR`, else `$XDG_STATE_HOME/devsrv`, else `~/.local/state/devsrv`.
    pub fn open() -> Result<Store, Failure> {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let root = env("DEVSRV_STATE_DIR")
            .or_else(|| env("XDG_STATE_HOME").map(|p| p.join("devsrv")))
            .or_else(|| env("HOME").map(|h| h.join(".local/state/devsrv")))
            .ok_or_else(|| Failure::system("HOME is not set").hint("set DEVSRV_STATE_DIR"))?;
        if !root.is_absolute() {
            return Err(Failure::usage(format!("DEVSRV_STATE_DIR must be an absolute path, not {}", root.display())));
        }
        mkdir(&root).map_err(|e| unwritable(&root, &e))?;
        Ok(Store { root })
    }

    /// The directory of one project, created on first use.
    pub fn project(&self, scope: &Scope) -> Result<Project, Failure> {
        let dir = self.root.join(scope.dir_name());
        mkdir(&dir).map_err(|e| unwritable(&dir, &e))?;
        let marker = dir.join("root");
        let want = scope.path().to_string_lossy().to_string();
        match fs::read_to_string(&marker) {
            Ok(have) if have == want => {}
            Ok(have) => {
                return Err(Failure::system(format!("{} belongs to {have}, not {want}", dir.display()))
                    .hint("set DEVSRV_STATE_DIR to another directory"));
            }
            Err(_) => fs::write(&marker, &want).map_err(|e| unwritable(&dir, &e))?,
        }
        Ok(Project { dir, scope: scope.clone() })
    }

    /// Every project that has a state directory.
    pub fn projects(&self) -> Vec<Project> {
        let Ok(entries) = fs::read_dir(&self.root) else { return vec![] };
        let mut out: Vec<Project> = entries
            .filter_map(|e| {
                let dir = e.ok()?.path();
                let root = fs::read_to_string(dir.join("root")).ok()?;
                Some(Project { dir, scope: Scope::from_recorded(root.into()) })
            })
            .collect();
        out.sort_by(|a, b| a.scope.path().cmp(b.scope.path()));
        out
    }
}

pub struct Project {
    dir: PathBuf,
    pub scope: Scope,
}

impl Project {
    pub fn instance(&self, name: &Name) -> Instance {
        Instance { dir: self.dir.join(name.as_str()), name: name.clone() }
    }

    pub fn instances(&self) -> Vec<Instance> {
        let Ok(entries) = fs::read_dir(&self.dir) else { return vec![] };
        let mut names: Vec<Name> = entries
            .filter_map(|e| {
                let e = e.ok()?;
                e.file_type().ok()?.is_dir().then_some(())?;
                Name::parse(e.file_name().to_str()?)
            })
            .collect();
        names.sort();
        names.iter().map(|n| self.instance(n)).collect()
    }
}

/// What a reader sees for a name, derived from the record and the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The lock is held and the command is not ready yet (or not spawned yet).
    Starting,
    Ready,
    /// The record is final.
    Down,
    /// The record says Up but nobody holds the lock: the supervisor was killed.
    Orphan,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Ready => "ready",
            Status::Down => "dead",
            Status::Orphan => "orphan",
        }
    }
}

pub struct Instance {
    dir: PathBuf,
    pub name: Name,
}

/// The name's lock is taken by someone else.
pub struct Taken;

impl Instance {
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }
    pub fn stop_path(&self) -> PathBuf {
        self.dir.join("stop")
    }
    pub fn supervisor_err_path(&self) -> PathBuf {
        self.dir.join("supervisor.err")
    }
    fn record_path(&self) -> PathBuf {
        self.dir.join("record.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }
    /// `Ok(None)` when there is no record. A record that cannot be read or parsed is an
    /// error (exit 3): nothing is decided, and nothing signalled, on a guess.
    pub fn read(&self) -> Result<Option<Record>, Failure> {
        let path = self.record_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Failure::system(format!("cannot read {}: {e}", path.display()))),
        };
        let broken = |why: String| {
            Failure::system(format!("the record of {} is broken: {why}", self.name))
                .detail(path.display().to_string())
                .hint(format!("if nothing of it runs any more, delete {} and start again", self.dir.display()))
        };
        let record: Record = serde_json::from_str(&text).map_err(|e| broken(e.to_string()))?;
        if record.v != RECORD_VERSION {
            return Err(broken(format!("version {} (this devsrv reads {RECORD_VERSION})", record.v)));
        }
        Ok(Some(record))
    }

    /// Is the lock held? A shared, non-blocking attempt that is released at once.
    pub fn lock_held(&self) -> bool {
        let Ok(f) = File::open(self.lock_path()) else { return false };
        // SAFETY: plain syscalls on a descriptor we own.
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        r != 0
    }

    pub fn status(&self) -> Result<Option<(Record, Status)>, Failure> {
        let held = self.lock_held();
        let record = self.read()?;
        let status = match (&record, held) {
            (Some(Record { state: State::Up { ready: Some(_), .. }, .. }), true) => Status::Ready,
            (Some(Record { state: State::Up { .. }, .. }), false) => Status::Orphan,
            (Some(Record { state: State::Down { .. }, .. }), false) => Status::Down,
            (_, true) => Status::Starting,
            (None, false) => return Ok(None),
        };
        Ok(record.map(|r| (r, status)))
    }

    /// Takes the name's lock. A reader's brief shared lock (`lock_held`) is waited out.
    pub fn claim(&self) -> Result<Result<Claim, Taken>, Failure> {
        mkdir(&self.dir).map_err(|e| unwritable(&self.dir, &e))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.lock_path())
            .map_err(|e| unwritable(&self.dir, &e))?;
        for _ in 0..10 {
            // SAFETY: plain syscall on a descriptor we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Ok(Claim { lock: file, dir: self.dir.clone() }));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(Err(Taken))
    }
}

/// The name's lock. Only its holder writes the record.
pub struct Claim {
    lock: File,
    dir: PathBuf,
}

impl Claim {
    pub fn write(&self, record: &Record) -> io::Result<()> {
        let tmp = self.dir.join("record.json.tmp");
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(record).map_err(io::Error::other)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("record.json"))
    }

    pub fn clear_record(&self) -> io::Result<()> {
        match fs::remove_file(self.dir.join("record.json")) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The descriptor, for handing the lock to the supervisor across exec.
    pub fn fd(&self) -> i32 {
        self.lock.as_raw_fd()
    }

    /// Takes over a lock descriptor inherited across exec (`DEVSRV_LOCK_FD`).
    ///
    /// # Safety
    /// `fd` must be an open descriptor of the name's lock file that nothing else owns.
    pub unsafe fn inherit(fd: i32, dir: PathBuf) -> Claim {
        use std::os::fd::FromRawFd;
        // SAFETY: the caller guarantees ownership; set close-on-exec again so the
        // command does not hold the lock.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Claim { lock: unsafe { File::from_raw_fd(fd) }, dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
