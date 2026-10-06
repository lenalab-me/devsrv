//! The side of devsrv that runs in the agent's shell. It reads records, takes the lock to
//! start, and leaves stop requests; the supervisor does the rest.

use std::fs::{self, File};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::{PortArg, StartArgs, WaitTarget};
use crate::model::{Cause, End, Name, Ready, Record, Scope, Spec, State, UnixMs, human};
use crate::os::{self, ExitStatus, Lookup, OwnedGroup};
use crate::probe::{self, Condition, Outcome};
use crate::report::{Failure, NOT_MET, Output, USAGE};
use crate::store::{Instance, Project, Status, Store};
use crate::supervisor;

const LOG_TAIL: usize = 20;

fn here() -> Result<(Store, Project), Failure> {
    let cwd = std::env::current_dir().map_err(|e| Failure::system(format!("cannot read the working directory: {e}")))?;
    let scope = Scope::of(&cwd).map_err(|e| Failure::system(format!("cannot resolve {}: {e}", cwd.display())))?;
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
        f = f.detail(format!("log (last {} of {total} lines): {}", lines.len(), path.display()));
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

/// One process as `ls`, `start` failures and `who` show it.
fn view(inst: &Instance, record: &Record, status: Status) -> Value {
    let spec = &record.spec;
    let mut v = json!({
        "name": inst.name.as_str(),
        "state": status.word(),
        "alive": matches!(status, Status::Starting | Status::Ready),
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
            if let End::Exited { pid, .. } = end {
                v["pid"] = json!(pid);
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
            let (msg, pid) = match &record.state {
                State::Up { child, ready: Some(r), .. } => (format!("{name} is already started: {} (pid {}, up {age})", r.address, child.pid), child.pid),
                State::Up { child, .. } => (format!("{name} is already starting (pid {}, for {age})", child.pid), child.pid),
                State::Down { .. } => (format!("{name} is being started by another devsrv"), 0),
            };
            let _ = pid;
            Failure::new(NOT_MET, "already_started", msg).hint(hint).data(view(inst, &record, status))
        }
        Ok(None) => Failure::new(NOT_MET, "already_started", format!("{name} is being started by another devsrv")).hint(hint),
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
        Ok(p) if !p.is_empty() => p.iter().map(|&p| owner_text(p)).collect::<Vec<_>>().join(", "),
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
    if let Some(Record { state: State::Up { child, .. }, .. }) = inst.read()? {
        match OwnedGroup::verify(&child) {
            Lookup::Found(_) => {
                return Err(Failure::new(NOT_MET, "already_started", format!("{name} is already running (pid {}), but its devsrv supervisor is gone", child.pid))
                    .hint(format!("devsrv stop {name} stops it; then start again")));
            }
            Lookup::Denied => return Err(Failure::system(format!("cannot tell whether pid {} of {name} still runs", child.pid))),
            Lookup::Gone => {}
        }
    }
    let port = match args.port {
        None => None,
        Some(PortArg::Auto) => {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| Failure::system(format!("cannot pick a free port: {e}")))?;
            Some(l.local_addr().map_err(|e| Failure::system(e.to_string()))?.port())
        }
        Some(PortArg::Fixed(p)) => {
            if port_in_use(p) {
                return Err(Failure::new(NOT_MET, "port_in_use", format!("port {p} is in use by {}; nothing was started", owners_text(p)))
                    .hint("use --port auto, or another port")
                    .hint(format!("devsrv who {p} tells whether devsrv started it")));
            }
            Some(p)
        }
    };
    let argv: Vec<String> = match port {
        Some(p) => args.argv.iter().map(|a| a.replace("{port}", &p.to_string())).collect(),
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
    };
    let fail_io = |what: &str, e: std::io::Error| Failure::system(format!("cannot {what}: {e}"));
    claim.clear_record().map_err(|e| fail_io("clear the old record", e))?;
    let _ = fs::remove_file(inst.stop_path());
    File::create(inst.log_path()).map_err(|e| fail_io("create the log", e))?;
    let sup_err = File::create(inst.supervisor_err_path()).map_err(|e| fail_io("create supervisor.err", e))?;
    let exe = std::env::current_exe().map_err(|e| fail_io("find the devsrv binary", e))?;
    let lock_fd = claim.fd();
    let mut cmd = Command::new(exe);
    cmd.arg("__supervise")
        .env(supervisor::LOCK_FD_ENV, lock_fd.to_string())
        .env(supervisor::DIR_ENV, claim.dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(sup_err);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            libc::fcntl(lock_fd, libc::F_SETFD, 0);
            Ok(())
        });
    }
    let mut sup = cmd.spawn().map_err(|e| fail_io("start the supervisor", e))?;
    let spec_json = serde_json::to_vec(&spec).map_err(|e| Failure::system(e.to_string()))?;
    if let Some(mut stdin) = sup.stdin.take() {
        stdin.write_all(&spec_json).map_err(|e| fail_io("hand the spec to the supervisor", e))?;
    }
    drop(claim);
    let started = Instant::now();
    let limit = args.timeout + Duration::from_secs(15);
    loop {
        match inst.read()? {
            Some(Record { state: State::Up { child, ready: Some(r), .. }, .. }) => {
                let note = format!("devsrv: {name} ready in {} (pid {}); logs: devsrv logs {name}; stop: devsrv stop {name}\n", human(Duration::from_millis(r.waited_ms)), child.pid);
                let data = json!({ "name": name.as_str(), "address": r.address, "pid": child.pid, "port": port, "waited_ms": r.waited_ms, "log": inst.log_path() });
                return Ok(Output::new(format!("{}\n", r.address), data).note(note));
            }
            Some(Record { state: State::Down { end, .. }, spec, .. }) => return Err(start_failure(&inst, &spec, &end)),
            _ => {}
        }
        if let Ok(Some(_)) = sup.try_wait()
            && !matches!(inst.read()?, Some(Record { state: State::Down { .. } | State::Up { ready: Some(_), .. }, .. }))
        {
            let err = fs::read_to_string(inst.supervisor_err_path()).unwrap_or_default();
            let mut f = Failure::system(format!("the devsrv supervisor of {name} stopped without a result"));
            for l in err.lines().take(10) {
                f = f.detail(l.to_string());
            }
            return Err(f);
        }
        if started.elapsed() > limit {
            return Err(Failure::system(format!("the devsrv supervisor of {name} did not report within {}", human(limit)))
                .hint(format!("devsrv ls shows its state; devsrv stop {name} stops it")));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start_failure(inst: &Instance, spec: &Spec, end: &End) -> Failure {
    let name = &inst.name;
    let after = human(spec.created.elapsed());
    match end {
        End::SpawnFailed { error, not_found } => {
            let f = Failure::new(USAGE, "cannot_run", format!("cannot run `{}`: {error}", spec.argv[0]));
            if *not_found {
                f.hint("check the command name and PATH; for a shell line use -- sh -c '...'")
            } else {
                f.hint("check that the command is executable")
            }
        }
        End::Exited { status, cause: Cause::OnItsOwn, .. } => with_log_tail(
            Failure::new(NOT_MET, "exited", format!("{name} {} after {after}, before it was ready", status_text(*status)))
                .hint("fix the command and start again; nothing is left running")
                .data(json!({ "exit": status })),
            inst,
        ),
        End::Exited { cause: Cause::StartTimedOut { last }, .. } => {
            let mut f = Failure::new(
                NOT_MET,
                "not_ready",
                format!("{name} was not ready within {} (last check: {last}); stopped it", human(spec.timeout())),
            );
            if let (Some(p), Ready::Http { .. } | Ready::Port) = (spec.port, &spec.ready) {
                f = f.hint(format!("if the command ignores $PORT, put {{port}} in its arguments (it was {p})"));
                if last.contains("HTTP") || last.contains("no HTTP") {
                    f = f.hint("if it is not an HTTP server, use --ready port");
                }
            }
            with_log_tail(f.hint("if it is just slow, raise --timeout"), inst)
        }
        End::Exited { cause: Cause::PortTaken { owners }, .. } => {
            let who = owners.iter().map(|&p| owner_text(p)).collect::<Vec<_>>().join(", ");
            let port = spec.port.unwrap_or(0);
            Failure::new(NOT_MET, "port_in_use", format!("port {port} answered, but it is held by {who}, not by {name}; stopped {name}"))
                .hint("start again with --port auto")
        }
        End::Exited { cause: Cause::Stopped { .. }, .. } => {
            Failure::new(NOT_MET, "stopped", format!("{name} was stopped (devsrv stop) before it was ready"))
        }
    }
}

fn line(inst: &Instance, record: &Record, status: Status) -> String {
    let spec = &record.spec;
    let port = spec.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
    let (pid, extra) = match &record.state {
        State::Up { child, ready, .. } => (child.pid.to_string(), ready.as_ref().map(|r| r.address.clone()).unwrap_or_default()),
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
    let extra = if status == Status::Orphan { format!("supervisor gone; devsrv stop {} stops it", inst.name) } else { extra };
    format!("{:<12} {:<8} {:>6} {:>5} {:>7}  {}  {}", inst.name.as_str(), status.word(), pid, port, human(spec.created.elapsed()), spec.cwd.display(), extra)
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
            let Some((record, status)) = inst.status()? else { continue };
            if matches!(status, Status::Starting | Status::Ready) {
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
            text.push_str(&format!("nothing started in {}\n", p.scope.path().display()));
            continue;
        }
        text.push_str(&format!("{alive} running, {dead} dead in {}\n", p.scope.path().display()));
        text.push_str(&format!("{:<12} {:<8} {:>6} {:>5} {:>7}  {}\n", "NAME", "STATE", "PID", "PORT", "AGE", "CWD"));
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
        Failure::new(NOT_MET, "unknown_name", format!("nothing named {} was started here", inst.name)).hint("devsrv ls shows the names")
    })
}

pub fn logs(name: Name, lines: usize, follow: Option<regex_lite::Regex>, timeout: Duration) -> Result<Output, Failure> {
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
        return Ok(Output::new(text, json!({ "name": name.as_str(), "log": path, "lines": tail, "total": total })));
    };
    let cond = Condition::Log { path: path.clone(), pattern };
    follow_log(&inst, cond, timeout)
}

/// Waits for a log line. Gives up early when the process is gone and the log is read.
fn follow_log(inst: &Instance, cond: Condition, timeout: Duration) -> Result<Output, Failure> {
    let name = &inst.name;
    let ended = || match inst.status() {
        Ok(Some((_, Status::Down | Status::Orphan))) | Ok(None) => Some(()),
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

/// Stops one name. `Ok(text)` or a failure.
fn stop_one(inst: &Instance) -> Result<(String, Value), Failure> {
    let name = &inst.name;
    let (record, status) = known(inst)?;
    let done = |text: String, how: &str| Ok((text, json!({ "name": name.as_str(), "result": how })));
    match (status, &record.state) {
        (Status::Down, State::Down { ended, .. }) => done(format!("{name}: already stopped ({} ago)", human(ended.elapsed())), "already_stopped"),
        (Status::Starting | Status::Ready, State::Up { child, .. }) => {
            fs::write(inst.stop_path(), child.token()).map_err(|e| Failure::system(format!("cannot write the stop request: {e}")))?;
            let t0 = Instant::now();
            while inst.lock_held() {
                if t0.elapsed() > Duration::from_secs(15) {
                    return Err(Failure::system(format!("the supervisor of {name} did not finish stopping it within 15s"))
                        .hint(format!("devsrv ls shows its state; devsrv logs {name} shows its output")));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            match inst.read()? {
                Some(Record { state: State::Down { end: End::Exited { survivors, cause, .. }, .. }, .. }) => {
                    if !survivors.is_empty() {
                        let pids = survivors.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
                        return Err(Failure::new(NOT_MET, "survivors", format!("{name}: sent KILL, but pids {pids} are still alive")));
                    }
                    let how = if matches!(cause, Cause::Stopped { forced: true }) { "KILL" } else { "TERM" };
                    let how = if matches!(cause, Cause::Stopped { .. }) { how } else { "it had exited" };
                    done(format!("{name}: stopped ({how}, {})", human(t0.elapsed())), "stopped")
                }
                _ => done(format!("{name}: stopped"), "stopped"),
            }
        }
        (Status::Orphan, State::Up { child, .. }) => {
            let claim = match inst.claim()? {
                Ok(c) => c,
                Err(_) => return Err(Failure::new(NOT_MET, "busy", format!("{name} is being started or stopped by another devsrv")).hint("try again")),
            };
            let (text, status, forced) = match OwnedGroup::verify(child) {
                Lookup::Found(group) => {
                    let t = group.terminate(Duration::from_secs(5));
                    if !t.survivors.is_empty() {
                        return Err(Failure::new(NOT_MET, "survivors", format!("{name}: sent KILL, but pids {:?} are still alive", t.survivors)));
                    }
                    (format!("{name}: stopped ({}; its supervisor was gone)", if t.forced { "KILL" } else { "TERM" }), "stopped", t.forced)
                }
                Lookup::Gone => (format!("{name}: already stopped (its supervisor was gone)"), "already_stopped", false),
                Lookup::Denied => return Err(Failure::system(format!("cannot tell whether pid {} of {name} is still the one devsrv started; nothing was sent", child.pid))),
            };
            let end = End::Exited { pid: child.pid, status: ExitStatus::Unknown, cause: Cause::Stopped { forced }, survivors: vec![] };
            let _ = claim.write(&Record { v: record.v, spec: record.spec.clone(), state: State::Down { ended: UnixMs::now(), end } });
            done(text, status)
        }
        _ => Err(Failure::new(NOT_MET, "busy", format!("{name} is being started by another devsrv")).hint("try again in a moment")),
    }
}

pub fn stop(names: Vec<Name>, all: bool) -> Result<Output, Failure> {
    let (_, project) = here()?;
    let insts: Vec<Instance> = if all {
        project
            .instances()
            .into_iter()
            .filter(|i| matches!(i.status(), Ok(Some((_, Status::Starting | Status::Ready | Status::Orphan)))))
            .collect()
    } else {
        names.iter().map(|n| project.instance(n)).collect()
    };
    if insts.is_empty() {
        return Ok(Output::new("nothing to stop\n", json!({ "stopped": [] })));
    }
    let (mut done, mut items, mut failed): (Vec<String>, Vec<Value>, Vec<Failure>) = (vec![], vec![], vec![]);
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
            for f in failed {
                worst = worst.detail(format!("also: {}", f.message));
            }
            for t in done {
                worst = worst.detail(format!("also: {t}"));
            }
            Err(worst)
        }
        None => Ok(Output::new(done.iter().map(|t| format!("{t}\n")).collect::<String>(), json!({ "stopped": items }))),
    }
}

pub fn who(port: u16) -> Result<Output, Failure> {
    let store = Store::open()?;
    let pids = os::tcp_listeners(port).map_err(|e| {
        Failure::system(format!("cannot tell who listens on port {port}: {e}")).hint("on macOS devsrv uses /usr/sbin/lsof; on Linux, /proc")
    })?;
    if pids.is_empty() {
        return Err(Failure::new(NOT_MET, "free", format!("{port}: free (nothing listens on it)")).data(json!({ "port": port, "listeners": [] })));
    }
    // Live devsrv processes, by the pgid their command leads.
    let mut ours = Vec::new();
    for p in store.projects() {
        for inst in p.instances() {
            if let Ok(Some((record, status @ (Status::Starting | Status::Ready)))) = inst.status()
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
        let mine = ours.iter().find(|(c, ..)| c.pid == pgid && matches!(os::proc_info(c.pid), Lookup::Found(p) if p.started == c.started));
        match mine {
            Some((_, inst, record, status)) => {
                let here = std::env::current_dir().ok().and_then(|d| Scope::of(&d).ok());
                let place = if here.as_ref() == Some(&record.spec.scope) { "this project".to_string() } else { record.spec.scope.path().display().to_string() };
                text.push_str(&format!("{port}: {} (devsrv, {place})  pid {pid}  {cmd}  up {}\n", inst.name, human(record.spec.created.elapsed())));
                let mut v = view(inst, record, *status);
                v["listener_pid"] = json!(pid);
                items.push(v);
            }
            None => {
                text.push_str(&format!("{port}: pid {pid}  {cmd}  (not started by devsrv; devsrv will not stop it)\n"));
                items.push(json!({ "listener_pid": pid, "command": cmd, "devsrv": false }));
            }
        }
    }
    Ok(Output::new(text, json!({ "port": port, "listeners": items })))
}

pub fn wait(target: WaitTarget, timeout: Duration) -> Result<Output, Failure> {
    let (cond, what, hint) = match target {
        WaitTarget::Url { host, port, path } => {
            let url = format!("http://{host}:{port}{path}");
            (Condition::Http { host: Some(host), port, path }, url, format!("devsrv who {port} shows who holds the port; devsrv ls shows what devsrv started"))
        }
        WaitTarget::Port(port) => (
            Condition::Port { host: None, port },
            format!("port {port}"),
            format!("devsrv ls shows what devsrv started; devsrv who {port} shows who holds the port"),
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
            (Condition::Gone { pid, started }, format!("pid {pid} to exit"), "it is still running".into())
        }
        WaitTarget::Log { name, pattern } => {
            let (_, project) = here()?;
            let inst = project.instance(&name);
            known(&inst)?;
            return follow_log(&inst, Condition::Log { path: inst.log_path(), pattern }, timeout);
        }
    };
    match probe::wait_for(&cond, timeout, || None::<()>) {
        Outcome::Met(met, waited) => Ok(Output::new(
            format!("ready after {}: {}\n", human(waited), met.what),
            json!({ "waited_ms": waited.as_millis() as u64, "what": met.what }),
        )),
        Outcome::TimedOut(last) => Err(Failure::new(NOT_MET, "timed_out", format!("{what}: not ready within {} (last check: {last})", human(timeout))).hint(hint)),
        Outcome::Aborted(()) => unreachable!("wait has no abort"),
    }
}
