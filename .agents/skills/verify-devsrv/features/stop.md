# stop

Stops only the process groups devsrv started: TERM, up to 5 s, then KILL. Twice is fine. Never signals a process whose pid was reused.

## Sub-features

- `stop-group` grandchildren in the group are gone afterwards.
- `stop-kill` a group that ignores TERM gets KILL (`stopped (KILL, …)`).
- `stop-twice` the second stop says `already stopped` with exit 0.
- `stop-unknown` a name never started: exit 1 and `devsrv ls` as the hint.
- `stop-orphan` the supervisor was killed: stop verifies the start time and stops the group itself.
- `stop-reused-pid` the record points at another process: nothing is sent; that process lives.
- `stop-all` `stop --all` stops everything running in this project only.

## How to get to it (user POV)

- `devsrv stop NAME...`, `devsrv stop --all`

## Driving it with drive.sh

- **stop-group.** `start tree -- sh -c 'sleep 300 & sleep 301 & wait'`; read the pid from `ls-after.txt` (it is the PGID); `--sh 'ps -eo pid,pgid,stat,command | awk -v p=<PGID> "NR==1||\$2==p"'` lists three processes (not `ps -g`, which selects by session on Linux). `stop tree`: exit 0, stdout `tree: stopped (TERM, …)`. The same `ps` line lists none.
- **stop-kill.** `start deaf -- sh -c "trap '' TERM; sleep 300 & wait"`, `stop deaf`: stdout `deaf: stopped (KILL, 5.…s)`; the group is empty in `ps`.
- **stop-twice.** `stop tree` again: exit 0, `tree: already stopped (… ago)`.
- **stop-unknown.** `stop nope`: exit 1, stderr `devsrv: nothing named nope was started here`.
- **stop-orphan.** `start o -- sleep 300`; take the supervisor pid from `state/*/o/record.json` (`.state.up.supervisor.pid`) and `--sh 'kill -KILL <that pid>'`; `ls` shows `o orphan`; `stop o` prints `o: stopped (TERM; its supervisor was gone)` and the same `ps -eo … | awk` line lists none.
- **stop-reused-pid.** Covered by `tests/cli.rs::c05_a_reused_pid_is_not_signalled` (it rewrites the record to point at a stranger and checks the stranger survives); run `cargo test --test cli c05` and keep its output.

## Gotchas

- `stop` returns when the supervisor has finished, so the group is already empty when it prints `stopped`.
- A process that left the group (its own `setsid`) is not followed; this is by design.
