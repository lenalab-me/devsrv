# wait and logs

`wait` replaces sleep loops: it returns as soon as the condition holds (exit 0, `ready after <secs>: …`) or fails at the timeout (exit 1). `logs` prints the tail of a name's log or waits for a line.

## Sub-features

- `wait-url` `wait http://HOST:PORT/PATH` (a status from 200 to 499).
- `wait-status` `wait URL --status 200` (also `200-299`, `2xx`, `200,304`): only those codes count, so a 404 page is waited for.
- `wait-port` `wait port N` (also `--port N`, a bare `N`).
- `wait-timeout` gives up in about `--timeout` seconds with the last check and a hint.
- `wait-file`, `wait-gone` a file appears; a pid exits (a reused pid counts as gone).
- `wait-file-regex` `wait file PATH REGEX`: a line of the file matches; the file may not exist yet. On a timeout, the end of the file.
- `wait-cmd` `wait [--timeout T] cmd -- CMD ARGS...`: CMD exits 0, run every 0.5s (`--interval`). On a timeout, the exit status and output of the last run that finished; a run still going at the deadline is killed with its process group.
- `logs-tail` `logs NAME -n N`, the last line counted without a final newline.
- `logs-follow` `logs NAME --follow-until REGEX [--timeout]` (also `wait log NAME REGEX`); fails early when the process is gone and no line matched.

## How to get to it (user POV)

- `devsrv wait [--timeout 30s] [--interval 0.5s] URL [--status 200] | port N | file PATH [REGEX] | log NAME REGEX | gone PID | cmd -- CMD ARGS...`
- `devsrv logs NAME [-n 50] [--follow-until REGEX [--timeout 30s]]`

## Driving it with drive.sh

- **wait-timeout.** `--sh 'time $DEVSRV wait port 1 --timeout 1'`: exit code 1 printed by the shell line (append `; echo exit=$?`), stderr `port 1: not ready within 1.0s (last check: connection refused)`, real time about 1 s.
- **wait-url.** After **start-http** from `start.md`: `wait <the address> --timeout 5`: exit 0, stdout `ready after 0.0s: http://127.0.0.1:<port>/ (HTTP 200)`.
- **wait-status.** After **start-http**: `wait <the address>nope.html --status 200 --timeout 1`: exit 1, stderr `…nope.html: not ready within 1.0s (last check: HTTP 404)`. Without `--status`: exit 0 (404 counts).
- **wait-file-regex.** `--sh '(sleep 1; echo built > b.log) & $DEVSRV wait file b.log built --timeout 5'`: exit 0, stdout `ready after 1.0s: built`. `wait file b.log nope --timeout 1`: exit 1 and `| built` under the message.
- **wait-cmd.** `--sh '$DEVSRV wait --timeout 1 cmd -- sh -c "echo no; exit 3"; echo exit=$?'`: `exit=1`, stderr `(last check: exit 3)` and `| no`. `wait --timeout 1 cmd -- sleep 5`: `still running at the deadline; killed it` after about 1 s.
- **logs-tail.** `start talk --ready log:line7 -- sh -c 'for i in 1 2 3 4 5 6 7; do echo line$i; done; sleep 300'`, then `logs talk -n 5`: stdout `line3` … `line7`.
- **logs-follow.** `logs talk --follow-until 'line[5]' --timeout 2`: exit 0, stdout `line5`. `logs talk --follow-until never --timeout 1`: exit 1, stderr `no line of talk's log matched within 1.0s` and the log tail.

## Gotchas

- `--follow-until` matches from the start of the current run's log, so a line printed before the call counts.
- `wait` on a URL checks the host given; `wait port N` tries 127.0.0.1 and ::1.
- After `cmd --`, everything is the command's: `wait cmd -- curl URL --timeout 5` gives `--timeout 5` to curl. Put devsrv's options before `cmd`.
- `wait file PATH REGEX` reads the whole file, so a line left by an earlier run matches at once.
