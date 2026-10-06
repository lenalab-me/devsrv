# start

Runs a command in the background in its own process group, waits until it is ready, and prints its address on stdout (one line). A supervisor stays behind to record the exit status and to do the stopping.

## Sub-features

- `start-http` `--port auto` with `{port}` in the arguments; `--ready http` by default when there is a port.
- `start-kinds` `--ready port`, `--ready log:REGEX`, `--ready none` (still running after 0.5 s).
- `start-twice` a second start of a running name fails (exit 1) with the address, pid and age, and `devsrv stop NAME` as the hint.
- `start-dies` a command that exits before it is ready: exit 1, its exit code and the log tail.
- `start-timeout` not ready in time: exit 1, the last check, hints about `{port}`/`--ready port`/`--timeout`, and nothing left running.
- `start-cannot-run` a command that does not exist: exit 2.
- `start-port-in-use` a fixed `--port` that someone holds: exit 1 with the owner, nothing started.
- `start-detached` the command keeps running after the calling shell exits.

## How to get to it (user POV)

- `devsrv start NAME [--port auto|N] [--ready …] [--timeout …] -- CMD ARGS...`

## Driving it with drive.sh

- **start-http.** `$D/drive.sh $S start web --port auto -- python3 -m http.server {port}`. Exit 0; stdout is exactly one line `http://127.0.0.1:<port>/`; stderr `devsrv: web ready in …`. Then `$D/drive.sh $S --sh 'curl -s -o /dev/null -w "%{http_code}" "$($DEVSRV --json ls | jq -r ".processes[0].address")"'` prints `200`.
- **start-twice.** Repeat the same start. Exit 1; stderr line 1 `devsrv: web is already started: http://127.0.0.1:<port>/ (pid <pid>, up <age>)`; a `hint:` line with `devsrv stop web`.
- **start-dies.** `$D/drive.sh $S start api -- sh -c 'echo boom >&2; exit 3'`. Exit 1; stderr line 1 `devsrv: api exited with code 3 after …, before it was ready`; a line `  | boom`; `ls-after.txt` shows `api dead … exited with code 3`.
- **start-timeout.** `$D/drive.sh $S start deaf --port auto --timeout 1 -- sleep 300`. Exit 1; line 1 ends `(last check: connection refused); stopped it`; `ls-after.txt` shows `deaf dead`; `--sh 'pgrep -f "sleep 300"'` finds no process of this session.
- **start-kinds.** `start talk --ready 'log:^ready' -- sh -c 'echo ready; sleep 300'` prints `pid <pid>`; `start tcp --port auto --ready port -- python3 -m http.server {port}` prints `127.0.0.1:<port>`.
- **start-detached.** `$D/drive.sh $S --sh 'sh -c "$DEVSRV start bg --port auto -- python3 -m http.server {port}; exit 0"; sleep 0.3; curl -s -o /dev/null -w "%{http_code}" "$($DEVSRV --json ls | jq -r ".processes[] | select(.name==\"bg\") | .address")"'` ends with `200`.

## Gotchas

- Do not write `$PORT` in the command line: the calling shell expands it before devsrv runs. Use `{port}`.
- A command that backgrounds its server and exits (`sh -c 'server &'`) is treated as exited; devsrv stops what is left of its group.
- `start` returns only after the record says ready, so `ls-after.txt` of the start step already shows `ready`.
