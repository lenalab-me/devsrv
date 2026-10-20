---
name: verify-devsrv
description: Drive the devsrv CLI the way an agent does (start a real server, wait, look, stop) in an isolated session with its own state directory, and keep proof of what it printed, exited with, and left running. Use to prove a change to devsrv works before calling it done, to reproduce a reported misbehavior, or to check a feature in features/ end to end.
---

# Verify devsrv

devsrv is a CLI, but what it starts outlives each call: a supervisor (`devsrv __supervise`) and the command it runs. So a verification is a session of several calls (start, then check, then stop), and the proof includes which processes were left alive. Each session has its own state directory and project directory under `target/verify-devsrv/<session>/`, so it never sees or touches the user's own `~/.local/state/devsrv/`, and sessions can run side by side.

## Launch

From the repo root:

```bash
cargo build                      # the binary under test: target/debug/devsrv
```

Ready when `target/debug/devsrv -V` prints the version in `Cargo.toml`. Verify the working tree's build, not the `devsrv` on PATH (`~/.cargo/bin/devsrv` is the installed one and may be older). For a release build, set `DEVSRV_BIN=target/release/devsrv` (a relative path is taken from where you run the script).

## Doctor

```bash
.agents/skills/verify-devsrv/scripts/doctor.sh
```

Read-only. Exit 0 and `OK` when the binary exists, its version matches `Cargo.toml`, and no file in `src/` is newer than it. It also names any earlier session that still has something running (`NOTE: session … still has N running`): clean that up before starting a new one, so leftover servers are not mistaken for this run's.

## Drive

```bash
S=start-http-$(date +%s)
.agents/skills/verify-devsrv/scripts/drive.sh $S start web --port auto -- python3 -m http.server {port}
.agents/skills/verify-devsrv/scripts/drive.sh $S --sh 'curl -s -o /dev/null -w "%{http_code}\n" "$($DEVSRV --json ls | jq -r ".processes[0].address")"'
.agents/skills/verify-devsrv/scripts/drive.sh $S stop web
.agents/skills/verify-devsrv/scripts/cleanup.sh $S
```

- `drive.sh SESSION ARGS...` runs `devsrv ARGS...` in the session's project dir (a directory with a `.git`, so it is its own scope) with `DEVSRV_STATE_DIR` set to the session's state dir. It prints a summary (args, exit code, stdout, stderr, and `devsrv ls` right after) and saves it as the next `step-NN/`.
- `drive.sh SESSION --sh 'line'` runs a shell line in the same place, with `$DEVSRV` pointing at the binary and `DEVSRV_STATE_DIR` set. Use it for the observation half of a proof: `curl` the address, list the group with `ps -eo pid,pgid,stat,command | awk -v p=PGID 'NR==1||$2==p'` (not `ps -g`: on Linux it selects by session, so it is empty even while the group lives), run the binary from a subshell that exits (`sh -c '$DEVSRV start …; exit'`).
- The script's own exit status is 0 even when devsrv fails: read `exit code:` in the summary.
- Feature recipes with exact commands and expected results are in `features/`; start at `features/README.md`. Agents read `devsrv --help` (a summary of 35 lines; it must stay within 40), then `devsrv help COMMAND` for one command and `devsrv help all` for the whole text.

## Evidence

Each step writes `target/verify-devsrv/<session>/step-NN/`: `args.txt`, `stdout.txt`, `stderr.txt`, `exit_code`, `ls-after.txt`, `summary.txt`. The session's `state/` holds the records and logs devsrv wrote (`state/<project>-<hash>/<name>/record.json`, `log`, `supervisor.err`). `target/` is gitignored.

Proof standards:

- Drive the real binary from a real directory, and observe the effect with a tool other than devsrv: `curl` for an address, `ps` for processes. `devsrv ls` saying "ready" is devsrv's claim, not the proof.
- Stopping is proved by the process group being empty in `ps` afterwards (no line with that PGID, zombies aside), not by the `stopped` message.
- "Did not signal X" is proved by X still running afterwards (`kill -0 PID` or `ps -p PID`), for a process the session started outside devsrv.
- A failure is proved by the exit code, the first line of stderr, and the state after (`ls-after.txt`: nothing left running when the message says so).
- For timing claims (`wait --timeout 1` gives up in about a second), wrap the call: `drive.sh S --sh 'time $DEVSRV wait port N --timeout 1'`.

## Cleanup

```bash
.agents/skills/verify-devsrv/scripts/cleanup.sh SESSION
```

Runs `devsrv stop --all` with the session's state dir and fails unless nothing of the session is still running afterwards. It keeps the evidence (`target/verify-devsrv/<session>/` including `cleanup.txt`). Never stop processes by name (`pkill`): stop only what the session started, through devsrv or by the PID the session recorded. Delete old sessions by hand with `rm -r target/verify-devsrv/<session>` when no longer needed, never as part of a verification.

## Isolation

Any number of sessions can run at once: each has its own state dir, project dir, and ports (`--port auto`). Do not point a session at the user's state dir, and do not use fixed ports: something of the user's may be listening there. To check behavior against a port someone else holds, open a listener in the session yourself (`python3 -m http.server 0 &` inside `--sh`, and kill that PID at the end of the same line).
