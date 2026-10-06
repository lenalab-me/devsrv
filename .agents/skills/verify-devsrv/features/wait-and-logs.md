# wait and logs

`wait` replaces sleep loops: it returns as soon as the condition holds (exit 0, `ready after <secs>: …`) or fails at the timeout (exit 1). `logs` prints the tail of a name's log or waits for a line.

## Sub-features

- `wait-url` `wait http://HOST:PORT/PATH` (a status from 200 to 499).
- `wait-port` `wait port N` (also `--port N`, a bare `N`).
- `wait-timeout` gives up in about `--timeout` seconds with the last check and a hint.
- `wait-file`, `wait-gone` a file appears; a pid exits (a reused pid counts as gone).
- `logs-tail` `logs NAME -n N`, the last line counted without a final newline.
- `logs-follow` `logs NAME --follow-until REGEX [--timeout]` (also `wait log NAME REGEX`); fails early when the process is gone and no line matched.

## How to get to it (user POV)

- `devsrv wait URL | port N | file PATH | log NAME REGEX | gone PID [--timeout 30s]`
- `devsrv logs NAME [-n 50] [--follow-until REGEX [--timeout 30s]]`

## Driving it with drive.sh

- **wait-timeout.** `--sh 'time $DEVSRV wait port 1 --timeout 1'`: exit code 1 printed by the shell line (append `; echo exit=$?`), stderr `port 1: not ready within 1.0s (last check: connection refused)`, real time about 1 s.
- **wait-url.** After **start-http** from `start.md`: `wait <the address> --timeout 5`: exit 0, stdout `ready after 0.0s: http://127.0.0.1:<port>/ (HTTP 200)`.
- **logs-tail.** `start talk --ready log:line7 -- sh -c 'for i in 1 2 3 4 5 6 7; do echo line$i; done; sleep 300'`, then `logs talk -n 5`: stdout `line3` … `line7`.
- **logs-follow.** `logs talk --follow-until 'line[5]' --timeout 2`: exit 0, stdout `line5`. `logs talk --follow-until never --timeout 1`: exit 1, stderr `no line of talk's log matched within 1.0s` and the log tail.

## Gotchas

- `--follow-until` matches from the start of the current run's log, so a line printed before the call counts.
- `wait` on a URL checks the host given; `wait port N` tries 127.0.0.1 and ::1.
