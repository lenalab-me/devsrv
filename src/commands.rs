//! The side of devsrv that runs in the agent's shell. It reads records, takes the lock to
//! start, and leaves stop requests; the supervisor does the rest.

use std::fs::{self, File};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::{PortArg, StartArgs, WaitTarget};
use crate::model::{Cause, End, Name, ProcRef, Ready, Record, Scope, Spec, State, UnixMs, human};
use crate::os::{self, ExitStatus, Lookup, OwnedGroup};
use crate::probe::{self, Condition, Outcome};
use crate::report::{Failure, NOT_MET, Output, SYSTEM, USAGE};
use crate::store::{Instance, Project, Status, Store};
use crate::supervisor;

const LOG_TAIL: usize = 20;

fn here() -> Result<(Store, Project), Failure> {
    let cwd = std::env::current_dir()
        .map_err(|e| Failure::system(format!("cannot read the working directory: {e}")))?;
    let scope = Scope::of(&cwd)
        .map_err(|e| Failure::system(format!("cannot resolve {}: {e}", cwd.display())))?;
    let store = Store::open()?;
    let project = store.project(&scope)?;
    Ok((store, project))
}

fn with_log_tail(mut f: Failure, inst: &Instance) -> Failure {
    let path = inst.log_path();
    let (lines, total) = probe::tail(&path, LOG_TAIL);
    if total == 0 {
        f = f.detail(format!("log: (empty) {}", path.display()));
    } else {
        f = f.detail(format!(
            "log (last {} of {total} lines): {}",
            lines.len(),
            path.display()
        ));
        for l in &lines {
            f = f.detail(format!("| {l}"));
        }
    }
    let data = json!({ "log": path, "log_tail": lines });
    let prev = std::mem::take(&mut f.data);
    f.data(merge(prev, data))
}

fn merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut x), Value::Object(y)) => {
            x.extend(y);
            Value::Object(x)
        }
        (Value::Null, b) => b,
        (a, _) => a,
    }
}

fn status_text(s: ExitStatus) -> String {
    match s {
        ExitStatus::Code(c) => format!("exited with code {c}"),
        ExitStatus::Signal(n) => format!("was killed by signal {n}"),
        ExitStatus::Unknown => "ended (exit status unknown)".into(),
    }
}

/// Is the command alive? For an orphan (its supervisor is gone) the OS is asked whether
/// the recorded command still runs with the recorded start time.
fn alive(record: &Record, status: Status) -> bool {
    match (status, &record.state) {
        (Status::Starting | Status::Ready, _) => true,
        (Status::Orphan, State::Up { child, .. }) => {
            !matches!(OwnedGroup::verify(child), Lookup::Gone)
        }
        _ => false,
    }
}

/// One process as `ls`, `start` failures and `who` show it.
fn view(inst: &Instance, record: &Record, status: Status) -> Value {
    let spec = &record.spec;
    let mut v = json!({
        "name": inst.name.as_str(),
        "state": status.word(),
        "alive": alive(record, status),
        "port": spec.port,
        "dir": spec.scope.path(),
        "cwd": spec.cwd,
        "command": spec.argv,
        "log": inst.log_path(),
        "age_s": spec.created.elapsed().as_secs_f64(),
    });
    match &record.state {
        State::Up { child, ready, .. } => {
            v["pid"] = json!(child.pid);
            v["address"] = json!(ready.as_ref().map(|r| r.address.clone()));
        }
        State::Down { ended, end } => {
            v["ended_s_ago"] = json!(ended.elapsed().as_secs_f64());
            v["end"] = serde_json::to_value(end).unwrap_or(Value::Null);
            // Not "pid": a dead record has nothing to signal.
            if let End::Exited { pid, .. } = end {
                v["last_pid"] = json!(pid);
            }
        }
    }
    v
}

fn already_started(inst: &Instance) -> Failure {
    let name = &inst.name;
    let hint = format!("keep using it, or restart it: devsrv stop {name}, then start again");
    match inst.status() {
        Ok(Some((record, status))) => {
            let age = human(record.spec.created.elapsed());
            let busy = format!("{name} is being started or stopped by another devsrv");
            let (msg, hint) = match &record.state {
                State::Up { child, .. }
                    if fs::read_to_string(inst.stop_path())
                        .is_ok_and(|t| t.trim() == child.token().trim()) =>
                {
                    (
                        format!("{name} is being stopped (pid {})", child.pid),
                        format!(
                            "wait for it, then start again: devsrv wait gone {}",
                            child.pid
                        ),
                    )
                }
                State::Up {
                    child,
                    ready: Some(r),
                    ..
                } if r.address.starts_with("pid ") => (
                    format!("{name} is already started (pid {}, up {age})", child.pid),
                    hint,
                ),
                State::Up {
                    child,
                    ready: Some(r),
                    ..
                } => (
                    format!(
                        "{name} is already started: {} (pid {}, up {age})",
                        r.address, child.pid
                    ),
                    hint,
                ),
                State::Up { child, .. } => (
                    format!("{name} is already starting (pid {}, for {age})", child.pid),
                    hint,
                ),
                State::Down { .. } => (busy, "try again in a moment".into()),
            };
            Failure::new(NOT_MET, "already_started", msg)
                .hint(hint)
                .data(view(inst, &record, status))
        }
        Ok(None) => Failure::new(
            NOT_MET,
            "already_started",
            format!("{name} is being started by another devsrv"),
        )
        .hint("try again in a moment"),
        Err(e) => e,
    }
}

/// A fixed port that already accepts connections, or cannot be bound, is in use.
fn port_in_use(port: u16) -> bool {
    let probe = |a: SocketAddr| TcpStream::connect_timeout(&a, Duration::from_millis(200)).is_ok();
    probe(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        || probe(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)))
        || TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err()
}

fn owner_text(pid: i32) -> String {
    match os::command_line(pid) {
        Some(c) => format!("pid {pid} ({c})"),
        None => format!("pid {pid}"),
    }
}

fn owners_text(port: u16) -> String {
    match os::tcp_listeners(port) {
        Ok(p) if !p.is_empty() => p
            .iter()
            .map(|&p| owner_text(p))
            .collect::<Vec<_>>()
            .join(", "),
        _ => "a process devsrv cannot see".into(),
    }
}

pub fn start(args: StartArgs) -> Result<Output, Failure> {
    let (_, project) = here()?;
    let inst = project.instance(&args.name);
    let name = args.name.clone();
    let claim = match inst.claim()? {
        Ok(c) => c,
        Err(_) => return Err(already_started(&inst)),
    };
    // We hold the lock, so whatever the record says, its supervisor is gone.
    if let Some(Record {
        state: State::Up { child, .. },
        ..
    }) = inst.read()?
    {
        match OwnedGroup::verify(&child) {
            Lookup::Found(_) => {
                return Err(Failure::new(
                    NOT_MET,
                    "already_started",
                    format!(
                        "{name} is already running (pid {}), but its devsrv supervisor is gone",
                        child.pid
                    ),
                )
                .hint(format!("devsrv stop {name} stops it; then start again")));
            }
            Lookup::Denied => {
                return Err(Failure::system(format!(
                    "cannot tell whether pid {} of {name} still runs",
                    child.pid
                )));
            }
            Lookup::Gone => {
                let left = match os::proc_info(child.pid) {
                    Lookup::Gone => os::group_members(child.pid).map(|m| !m.is_empty()),
                    _ => Ok(false),
                };
                let left = left.map_err(|e| {
                    Failure::system(format!(
                        "cannot read what is left of the last run of {name}: {e}"
                    ))
                })?;
                if left {
                    return Err(Failure::new(
                        NOT_MET,
                        "leftovers",
                        format!(
                            "the last run of {name} left processes in its process group (pgid {})",
                            child.pid
                        ),
                    )
                    .hint(format!("devsrv stop {name} names them; then start again")));
                }
            }
        }
    }
    let port = match args.port {
        None => None,
        Some(PortArg::Auto) => {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .map_err(|e| Failure::system(format!("cannot pick a free port: {e}")))?;
            Some(
                l.local_addr()
                    .map_err(|e| Failure::system(e.to_string()))?
                    .port(),
            )
        }
        Some(PortArg::Fixed(p)) => {
            if port_in_use(p) {
                return Err(Failure::new(
                    NOT_MET,
                    "port_in_use",
                    format!(
                        "port {p} is in use by {}; nothing was started",
                        owners_text(p)
                    ),
                )
                .hint("use --port auto, or another port")
                .hint(format!("devsrv who {p} tells whether devsrv started it")));
            }
            Some(p)
        }
    };
    let argv: Vec<String> = match port {
        Some(p) => args
            .argv
            .iter()
            .map(|a| a.replace("{port}", &p.to_string()))
            .collect(),
        None => args.argv.clone(),
    };
    let cwd = std::env::current_dir().map_err(|e| Failure::system(e.to_string()))?;
    let spec = Spec {
        name: name.clone(),
        scope: project.scope.clone(),
        cwd,
        argv,
        port,
        ready: args.ready.clone(),
        timeout_ms: args.timeout.as_millis() as u64,
        created: UnixMs::now(),
        run: format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ),
    };
    let fail_io = |what: &str, e: std::io::Error| Failure::system(format!("cannot {what}: {e}"));
    claim
        .clear_record()
        .map_err(|e| fail_io("clear the old record", e))?;
    let _ = fs::remove_file(inst.stop_path());
    File::create(inst.log_path()).map_err(|e| fail_io("create the log", e))?;
    let mut sup =
        supervisor::launch(&claim, &spec).map_err(|e| fail_io("start the supervisor", e))?;
    drop(claim);
    let started = Instant::now();
    let limit = args.timeout + Duration::from_secs(15);
    loop {
        // Only this run's record counts: once it is Down, another start may replace it.
        let mine = inst.read()?.filter(|r| r.spec.run == spec.run);
        match mine {
            Some(Record {
                state:
                    State::Up {
                        child,
                        ready: Some(r),
                        ..
                    },
                ..
            }) => {
                let note = format!(
                    "devsrv: {name} ready in {} (pid {}); logs: devsrv logs {name}; stop: devsrv stop {name}\n",
                    human(Duration::from_millis(r.waited_ms)),
                    child.pid
                );
                let data = json!({ "name": name.as_str(), "address": r.address, "pid": child.pid, "port": port, "waited_ms": r.waited_ms, "log": inst.log_path() });
                return Ok(Output::new(format!("{}\n", r.address), data).note(note));
            }
            Some(Record {
                state: State::Down { end, .. },
                spec,
                ..
            }) => return Err(start_failure(&inst, &spec, &end)),
            _ => {}
        }
        if let Ok(Some(_)) = sup.try_wait()
            && !matches!(
                inst.read()?.filter(|r| r.spec.run == spec.run),
                Some(Record {
                    state: State::Down { .. } | State::Up { ready: Some(_), .. },
                    ..
                })
            )
        {
            if inst.read()?.is_some_and(|r| r.spec.run != spec.run) {
                return Err(Failure::new(
                    NOT_MET,
                    "replaced",
                    format!("{name} ended, and was started again by another devsrv start before this one saw how"),
                )
                .hint("devsrv ls shows what runs now"));
            }
            let err = fs::read_to_string(inst.supervisor_err_path()).unwrap_or_default();
            let mut f = Failure::system(format!(
                "the devsrv supervisor of {name} stopped without a result"
            ));
            for l in err.lines().take(10) {
                f = f.detail(l.to_string());
            }
            return Err(f);
        }
        if started.elapsed() > limit {
            return Err(Failure::system(format!(
                "the devsrv supervisor of {name} did not report within {}",
                human(limit)
            ))
            .hint(format!(
                "devsrv ls shows its state; devsrv stop {name} stops it"
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What is left of a run that ended, when something is: "" when nothing is.
fn leftovers(end: &End) -> Option<String> {
    match end {
        End::Exited { survivors, .. } if !survivors.is_empty() => Some(format!(
            "pids {} of its group are still alive after KILL",
            survivors
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        End::Exited {
            unconfirmed: true, ..
        } => Some("devsrv could not read its process group to confirm that nothing is left".into()),
        _ => None,
    }
}

fn start_failure(inst: &Instance, spec: &Spec, end: &End) -> Failure {
    let f = start_failure_cause(inst, spec, end);
    match leftovers(end) {
        Some(left) => Failure {
            code: f.code.max(NOT_MET),
            ..f
        }
        .detail(format!("warning: {left}")),
        None if matches!(end, End::Exited { .. }) => f.hint("nothing of it is left running"),
        None => f,
    }
}

fn start_failure_cause(inst: &Instance, spec: &Spec, end: &End) -> Failure {
    let name = &inst.name;
    let after = human(spec.created.elapsed());
    match end {
        End::SpawnFailed { error, not_found } => {
            let f = Failure::new(
                USAGE,
                "cannot_run",
                format!("cannot run `{}`: {error}", spec.argv[0]),
            );
            if *not_found {
                f.hint("check the command name and PATH; for a shell line use -- sh -c '...'")
            } else {
                f.hint("check that the command is executable")
            }
        }
        End::Exited {
            status,
            cause: Cause::OnItsOwn,
            ..
        } => with_log_tail(
            Failure::new(
                NOT_MET,
                "exited",
                format!(
                    "{name} {} after {after}, before it was ready",
                    status_text(*status)
                ),
            )
            .hint("fix the command and start again")
            .data(json!({ "exit": status })),
            inst,
        ),
        End::Exited {
            cause: Cause::StartTimedOut { last },
            ..
        } => {
            let mut f = Failure::new(
                NOT_MET,
                "not_ready",
                format!(
                    "{name} was not ready within {} (last check: {last}); stopped it",
                    human(spec.timeout())
                ),
            );
            if let (Some(p), Ready::Http { .. } | Ready::Port) = (spec.port, &spec.ready) {
                f = f.hint(format!(
                    "if the command ignores $PORT, put {{port}} in its arguments (it was {p})"
                ));
                if last.contains("HTTP") || last.contains("no HTTP") {
                    f = f.hint("if it is not an HTTP server, use --ready port");
                }
            }
            with_log_tail(f.hint("if it is just slow, raise --timeout"), inst)
        }
        End::Exited {
            cause: Cause::PortTaken { owners },
            ..
        } => {
            let who = owners
                .iter()
                .map(|&p| owner_text(p))
                .collect::<Vec<_>>()
                .join(", ");
            let port = spec.port.unwrap_or(0);
            Failure::new(
                NOT_MET,
                "port_in_use",
                format!(
                    "port {port} answered, but it is held by {who}, not by {name}; stopped {name}"
                ),
            )
            .hint("start again with --port auto")
        }
        End::Exited {
            cause: Cause::Stopped { .. },
            ..
        } => Failure::new(
            NOT_MET,
            "stopped",
            format!("{name} was stopped (devsrv stop) before it was ready"),
        ),
    }
}

fn line(inst: &Instance, record: &Record, status: Status) -> String {
    let spec = &record.spec;
    let port = spec
        .port
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    let (pid, extra) = match &record.state {
        State::Up { child, ready, .. } => (
            child.pid.to_string(),
            ready
                .as_ref()
                .map(|r| r.address.clone())
                .unwrap_or_default(),
        ),
        State::Down { ended, end } => match end {
            End::SpawnFailed { error, .. } => ("-".into(), format!("could not run: {error}")),
            End::Exited { status, cause, .. } => {
                let why = match cause {
                    Cause::OnItsOwn => status_text(*status),
                    Cause::StartTimedOut { .. } => "not ready in time".into(),
                    Cause::PortTaken { .. } => "port held by another process".into(),
                    Cause::Stopped { .. } => "stopped".into(),
                };
                ("-".into(), format!("{why}, {} ago", human(ended.elapsed())))
            }
        },
    };
    let extra = match status {
        Status::Orphan if alive(record, status) => {
            format!("supervisor gone; devsrv stop {} stops it", inst.name)
        }
        Status::Orphan => "supervisor gone; the command is gone too".into(),
        _ => extra,
    };
    format!(
        "{:<12} {:<8} {:>6} {:>5} {:>7}  {}  {}",
        inst.name.as_str(),
        status.word(),
        pid,
        port,
        human(spec.created.elapsed()),
        spec.cwd.display(),
        extra
    )
    .trim_end()
    .to_string()
}

pub fn ls(all: bool) -> Result<Output, Failure> {
    let (store, here) = here()?;
    let projects: Vec<Project> = if all { store.projects() } else { vec![here] };
    let mut text = String::new();
    let mut items = Vec::new();
    for p in &projects {
        let mut rows = Vec::new();
        let (mut alive, mut dead) = (0, 0);
        for inst in p.instances() {
            let Some((record, status)) = inst.status()? else {
                continue;
            };
            if self::alive(&record, status) {
                alive += 1;
            } else {
                dead += 1;
            }
            rows.push(line(&inst, &record, status));
            items.push(view(&inst, &record, status));
        }
        if rows.is_empty() && all {
            continue;
        }
        if rows.is_empty() {
            text.push_str(&format!(
                "nothing started in {}\n",
                p.scope.path().display()
            ));
            continue;
        }
        text.push_str(&format!(
            "{alive} running, {dead} dead in {}\n",
            p.scope.path().display()
        ));
        text.push_str(&format!(
            "{:<12} {:<8} {:>6} {:>5} {:>7}  {}\n",
            "NAME", "STATE", "PID", "PORT", "AGE", "CWD"
        ));
        for r in rows {
            text.push_str(&r);
            text.push('\n');
        }
    }
    if text.is_empty() {
        text = "nothing started\n".into();
    }
    Ok(Output::new(text, json!({ "processes": items })))
}

fn known(inst: &Instance) -> Result<(Record, Status), Failure> {
    inst.status()?.ok_or_else(|| {
        Failure::new(
            NOT_MET,
            "unknown_name",
            format!("nothing named {} was started here", inst.name),
        )
        .hint("devsrv ls shows the names")
    })
}

pub fn logs(
    name: Name,
    lines: usize,
    follow: Option<regex_lite::Regex>,
    timeout: Duration,
) -> Result<Output, Failure> {
    let (_, project) = here()?;
    let inst = project.instance(&name);
    known(&inst)?;
    let path = inst.log_path();
    let Some(pattern) = follow else {
        let (tail, total) = probe::tail(&path, lines);
        let mut text = tail.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        return Ok(Output::new(
            text,
            json!({ "name": name.as_str(), "log": path, "lines": tail, "total": total }),
        ));
    };
    let cond = Condition::Log {
        path: path.clone(),
        pattern,
    };
    follow_log(&inst, cond, timeout)
}

/// Waits for a log line. Gives up early when the process is gone and the log is read.
fn follow_log(inst: &Instance, cond: Condition, timeout: Duration) -> Result<Output, Failure> {
    let name = &inst.name;
    let ended = || match inst.status() {
        Ok(Some((record, status))) if !alive(&record, status) => Some(()),
        Ok(None) => Some(()),
        _ => None,
    };
    let mut gone_once = false;
    let outcome = probe::wait_for(&cond, timeout, || {
        // One more look at the log after the process is seen gone.
        if gone_once {
            return Some(());
        }
        gone_once = ended().is_some();
        None
    });
    match outcome {
        Outcome::Met(met, waited) => Ok(Output::new(format!("{}\n", met.what), json!({ "name": name.as_str(), "line": met.what, "waited_ms": waited.as_millis() as u64 }))
            .note(format!("devsrv: matched after {}\n", human(waited)))),
        Outcome::TimedOut(_) => Err(with_log_tail(
            Failure::new(NOT_MET, "timed_out", format!("no line of {name}'s log matched within {}", human(timeout))).hint("check the pattern against the log above, or raise --timeout"),
            inst,
        )),
        Outcome::Aborted(()) => Err(with_log_tail(
            Failure::new(NOT_MET, "exited", format!("{name} is not running and no line of its log matched")).hint(format!("devsrv ls shows how {name} ended")),
            inst,
        )),
    }
}

/// The result of a stop, from the final record of the run that was stopped.
fn stopped(
    name: &Name,
    end: &End,
    how_long: Duration,
    note: &str,
) -> Result<(String, Value), Failure> {
    if let Some(left) = leftovers(end) {
        let code = if matches!(
            end,
            End::Exited {
                unconfirmed: true,
                ..
            }
        ) {
            SYSTEM
        } else {
            NOT_MET
        };
        return Err(Failure::new(
            code,
            "not_confirmed",
            format!("{name}: {left}"),
        ));
    }
    let how = match end {
        End::Exited {
            cause: Cause::Stopped { forced: true },
            ..
        } => "KILL",
        End::Exited {
            cause: Cause::Stopped { forced: false },
            ..
        } => "TERM",
        _ => "it had exited",
    };
    Ok((
        format!("{name}: stopped ({how}, {}{note})", human(how_long)),
        json!({ "name": name.as_str(), "result": "stopped", "how": how }),
    ))
}

/// Stops a name whose supervisor is gone: the command is checked against its recorded
/// start time before every signal, and the final record is written here.
fn stop_orphan(
    inst: &Instance,
    record: &Record,
    child: &ProcRef,
) -> Result<(String, Value), Failure> {
    let name = &inst.name;
    let Ok(claim) = inst.claim()? else {
        return Err(Failure::new(
            NOT_MET,
            "busy",
            format!("{name} is being started or stopped by another devsrv"),
        )
        .hint("try again in a moment"));
    };
    let t0 = Instant::now();
    let (cause, t) = match OwnedGroup::verify(child) {
        Lookup::Found(group) => {
            let t = group.terminate_verified(Duration::from_secs(5));
            (Cause::Stopped { forced: t.forced }, t)
        }
        Lookup::Gone => {
            // The leader is gone. What is left in its group cannot be proved to be ours
            // (the group id could have been reused), so it is named, not signalled. When
            // the pid now belongs to another process, the old group was empty when it was
            // reused (a pid is not handed out while a group of that id exists): nothing
            // of ours is left to name.
            let left: Vec<i32> = match os::proc_info(child.pid) {
                Lookup::Gone => os::group_members(child.pid)
                    .map_err(|e| Failure::system(format!("cannot read what is left of {name}'s process group: {e}; nothing was sent")))?
                    .into_iter()
                    .map(|p| p.pid)
                    .collect(),
                _ => vec![],
            };
            if !left.is_empty() {
                let end = End::Exited {
                    pid: child.pid,
                    status: ExitStatus::Unknown,
                    cause: Cause::OnItsOwn,
                    survivors: left.clone(),
                    unconfirmed: false,
                };
                let down = Record {
                    v: record.v,
                    spec: record.spec.clone(),
                    state: State::Down {
                        ended: UnixMs::now(),
                        end,
                    },
                };
                claim.write(&down).map_err(|e| {
                    Failure::system(format!("cannot write the record of {name}: {e}"))
                })?;
                let mut f = Failure::new(
                    NOT_MET,
                    "survivors",
                    format!(
                        "{name}: its command (pid {}) is gone, but its process group still has processes; devsrv cannot prove they are its own, so it sent nothing",
                        child.pid
                    ),
                );
                for p in &left {
                    f = f.detail(owner_text(*p));
                }
                return Err(f
                    .hint(format!(
                        "if these are what {name} left behind: kill -TERM -{}",
                        child.pid
                    ))
                    .data(json!({ "name": name.as_str(), "pgid": child.pid, "survivors": left })));
            }
            (Cause::OnItsOwn, os::Terminated::default())
        }
        Lookup::Denied => {
            return Err(Failure::system(format!(
                "cannot tell whether pid {} of {name} is still the process devsrv started; nothing was sent",
                child.pid
            )));
        }
    };
    let gone_before = matches!(cause, Cause::OnItsOwn);
    let end = End::Exited {
        pid: child.pid,
        status: ExitStatus::Unknown,
        cause,
        survivors: t.survivors,
        unconfirmed: t.unconfirmed,
    };
    let down = Record {
        v: record.v,
        spec: record.spec.clone(),
        state: State::Down {
            ended: UnixMs::now(),
            end: end.clone(),
        },
    };
    claim
        .write(&down)
        .map_err(|e| Failure::system(format!("cannot write the record of {name}: {e}")))?;
    if gone_before {
        return Ok((
            format!("{name}: already stopped (its supervisor was gone)"),
            json!({ "name": name.as_str(), "result": "already_stopped" }),
        ));
    }
    stopped(name, &end, t0.elapsed(), "; its supervisor was gone")
}

/// Stops one name. Reads the record again on every turn instead of deciding once: the
/// supervisor may die while stopping, and a start may be between taking the lock and
/// writing its first record.
fn stop_one(inst: &Instance) -> Result<(String, Value), Failure> {
    let name = &inst.name;
    let t0 = Instant::now();
    // The run this call asked to stop (the command, by pid and start time).
    let mut requested: Option<ProcRef> = None;
    loop {
        if t0.elapsed() > Duration::from_secs(20) {
            return Err(
                Failure::system(format!("{name} did not stop within 20s")).hint(format!(
                    "devsrv ls shows its state; devsrv logs {name} shows its output"
                )),
            );
        }
        match inst.status()? {
            None if inst.lock_held() => {}
            None if requested.is_some() => {
                return Ok((
                    format!("{name}: stopped"),
                    json!({ "name": name.as_str(), "result": "stopped" }),
                ));
            }
            None => {
                return Err(Failure::new(
                    NOT_MET,
                    "unknown_name",
                    format!("nothing named {name} was started here"),
                )
                .hint("devsrv ls shows the names"));
            }
            Some((record, status)) => match (&record.state, status) {
                (State::Down { end, ended }, Status::Down) => {
                    return match &requested {
                        Some(_) => stopped(name, end, t0.elapsed(), ""),
                        None => Ok((
                            format!("{name}: already stopped ({} ago)", human(ended.elapsed())),
                            json!({ "name": name.as_str(), "result": "already_stopped" }),
                        )),
                    };
                }
                (State::Up { child, .. }, Status::Orphan) => {
                    if requested.as_ref().is_some_and(|r| r != child) {
                        return Ok((
                            format!("{name}: stopped"),
                            json!({ "name": name.as_str(), "result": "stopped" }),
                        ));
                    }
                    return stop_orphan(inst, &record, child);
                }
                (State::Up { child, .. }, Status::Starting | Status::Ready) => match &requested {
                    None => {
                        fs::write(inst.stop_path(), child.token()).map_err(|e| {
                            Failure::system(format!("cannot write the stop request: {e}"))
                        })?;
                        requested = Some(child.clone());
                    }
                    Some(r) if r != child => {
                        return Ok((
                            format!(
                                "{name}: stopped; it has been started again since (pid {})",
                                child.pid
                            ),
                            json!({ "name": name.as_str(), "result": "stopped" }),
                        ));
                    }
                    Some(_) => {}
                },
                // A start holds the lock and has not written its record yet.
                _ => {}
            },
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn stop(names: Vec<Name>, all: bool) -> Result<Output, Failure> {
    let (_, project) = here()?;
    let insts: Vec<Instance> = if all {
        project
            .instances()
            .into_iter()
            // A record that cannot be read is kept, so that its error is reported.
            .filter(|i| match i.status() {
                Ok(Some((record, status))) => alive(&record, status),
                Ok(None) => i.lock_held(),
                Err(_) => true,
            })
            .collect()
    } else {
        names.iter().map(|n| project.instance(n)).collect()
    };
    if insts.is_empty() {
        return Ok(Output::new("nothing to stop\n", json!({ "stopped": [] })));
    }
    let (mut done, mut items, mut failed): (Vec<String>, Vec<Value>, Vec<Failure>) =
        (vec![], vec![], vec![]);
    for inst in &insts {
        match stop_one(inst) {
            Ok((t, v)) => {
                done.push(t);
                items.push(v);
            }
            Err(f) => failed.push(f),
        }
    }
    // With several names, the worst failure decides; the other results go along with it.
    failed.sort_by_key(|f| std::cmp::Reverse(f.code));
    let mut failed = failed.into_iter();
    match failed.next() {
        Some(mut worst) => {
            let failed: Vec<Value> = std::iter::once(&worst)
                .chain(failed.as_slice())
                .map(|f| json!({ "error": f.kind, "message": f.message }))
                .collect();
            for f in &failed[1..] {
                worst = worst.detail(format!("also: {}", f["message"].as_str().unwrap_or("")));
            }
            for t in done {
                worst = worst.detail(format!("also: {t}"));
            }
            let prev = std::mem::take(&mut worst.data);
            Err(worst.data(merge(prev, json!({ "stopped": items, "failed": failed }))))
        }
        None => Ok(Output::new(
            done.iter().map(|t| format!("{t}\n")).collect::<String>(),
            json!({ "stopped": items }),
        )),
    }
}

pub fn who(port: u16) -> Result<Output, Failure> {
    let store = Store::open()?;
    let pids = os::tcp_listeners(port).map_err(|e| {
        Failure::system(format!("cannot tell who listens on port {port}: {e}"))
            .hint("on macOS devsrv uses /usr/sbin/lsof; on Linux, /proc")
    })?;
    if pids.is_empty() {
        return Err(Failure::new(
            NOT_MET,
            "free",
            format!("{port}: free (nothing listens on it)"),
        )
        .data(json!({ "port": port, "listeners": [] })));
    }
    // Live devsrv processes, by the pgid their command leads.
    let mut ours = Vec::new();
    for p in store.projects() {
        for inst in p.instances() {
            if let Ok(Some((record, status))) = inst.status()
                && alive(&record, status)
                && let State::Up { child, .. } = &record.state
            {
                ours.push((child.clone(), inst, record.clone(), status));
            }
        }
    }
    let (mut text, mut items) = (String::new(), Vec::new());
    for pid in pids {
        let cmd = os::command_line(pid).unwrap_or_default();
        let pgid = match os::proc_info(pid) {
            Lookup::Found(p) => p.pgid,
            _ => 0,
        };
        let mine = ours.iter().find(|(c, ..)| {
            c.pid == pgid
                && matches!(os::proc_info(c.pid), Lookup::Found(p) if p.started == c.started)
        });
        match mine {
            Some((_, inst, record, status)) => {
                let here = std::env::current_dir()
                    .ok()
                    .and_then(|d| Scope::of(&d).ok());
                let place = if here.as_ref() == Some(&record.spec.scope) {
                    "this project".to_string()
                } else {
                    record.spec.scope.path().display().to_string()
                };
                let orphan = if *status == Status::Orphan {
                    format!("  (supervisor gone; devsrv stop {} stops it)", inst.name)
                } else {
                    String::new()
                };
                text.push_str(&format!(
                    "{port}: {} (devsrv, {place})  pid {pid}  {cmd}  up {}{orphan}\n",
                    inst.name,
                    human(record.spec.created.elapsed())
                ));
                let mut v = view(inst, record, *status);
                v["listener_pid"] = json!(pid);
                items.push(v);
            }
            None => {
                text.push_str(&format!(
                    "{port}: pid {pid}  {cmd}  (not started by devsrv; devsrv will not stop it)\n"
                ));
                items.push(json!({ "listener_pid": pid, "command": cmd, "devsrv": false }));
            }
        }
    }
    Ok(Output::new(
        text,
        json!({ "port": port, "listeners": items }),
    ))
}

pub fn wait(target: WaitTarget, timeout: Duration) -> Result<Output, Failure> {
    let (cond, what, hint) = match target {
        WaitTarget::Url { host, port, path } => {
            let url = format!("http://{host}:{port}{path}");
            (
                Condition::Http {
                    host: Some(host),
                    port,
                    path,
                },
                url,
                format!(
                    "devsrv who {port} shows who holds the port; devsrv ls shows what devsrv started"
                ),
            )
        }
        WaitTarget::Port(port) => (
            Condition::Port { host: None, port },
            format!("port {port}"),
            format!(
                "devsrv ls shows what devsrv started; devsrv who {port} shows who holds the port"
            ),
        ),
        WaitTarget::File(p) => {
            let w = p.display().to_string();
            (Condition::File(p), w, "check the path".into())
        }
        WaitTarget::Gone(pid) => {
            let started = match os::proc_info(pid) {
                Lookup::Found(p) => Some(p.started),
                Lookup::Gone => None,
                Lookup::Denied => return Err(Failure::system(format!("cannot observe pid {pid}"))),
            };
            (
                Condition::Gone { pid, started },
                format!("pid {pid} to exit"),
                "it is still running".into(),
            )
        }
        WaitTarget::Log { name, pattern } => {
            let (_, project) = here()?;
            let inst = project.instance(&name);
            known(&inst)?;
            return follow_log(
                &inst,
                Condition::Log {
                    path: inst.log_path(),
                    pattern,
                },
                timeout,
            );
        }
    };
    match probe::wait_for(&cond, timeout, || None::<()>) {
        Outcome::Met(met, waited) => Ok(Output::new(
            format!("ready after {}: {}\n", human(waited), met.what),
            json!({ "waited_ms": waited.as_millis() as u64, "what": met.what }),
        )),
        Outcome::TimedOut(last) => Err(Failure::new(
            NOT_MET,
            "timed_out",
            format!(
                "{what}: not ready within {} (last check: {last})",
                human(timeout)
            ),
        )
        .hint(hint)),
        Outcome::Aborted(()) => unreachable!("wait has no abort"),
    }
}
