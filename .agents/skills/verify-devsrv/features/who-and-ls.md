# who and ls

`who PORT` names the listener: a devsrv name (with its project) or a pid and command marked "not started by devsrv". It never stops anything. `ls` lists what devsrv started in this project (or `--all`).

## Sub-features

- `who-devsrv` a port held by a devsrv command, matched by process group and start time.
- `who-foreign` a port held by anyone else: pid, command, and "devsrv will not stop it".
- `who-free` nothing listens: exit 1, `free`.
- `ls-text` header `N running, M dead in <project>`, then one row per name with state, pid, port, age, cwd, address or how it ended.
- `ls-json` `--json ls`: `processes[]` with `name`, `state`, `alive`, `pid`, `port`, `address`, `age_s`, `cwd`, `log`, `end`.
- `ls-projects` the same name in two git top levels does not collide; `ls --all` shows every project.

## How to get to it (user POV)

- `devsrv who PORT`, `devsrv ls [--all]`, `devsrv --json ls`

## Driving it with drive.sh

- **who-devsrv.** After **start-http**: `who <port>`: exit 0, stdout `<port>: web (devsrv, this project)  pid <pid>  …python… -m http.server <port>  up …`.
- **who-foreign.** `--sh 'python3 -m http.server 0 >/dev/null 2>&1 & P=$!; sleep 0.5; PORT=$(lsof -nP -a -p $P -iTCP -sTCP:LISTEN -Fn | sed -n "s/.*://p" | head -1); $DEVSRV who $PORT; echo exit=$?; kill $P'`: the line ends `(not started by devsrv; devsrv will not stop it)`, `exit=0`, and the server was alive until the shell line killed it.
- **ls-json.** `--json ls`: one object; `processes[0].alive` is `true` for a ready name and `false` with `end.exited.status.code` for a dead one.

## Gotchas

- On macOS `who` asks `/usr/sbin/lsof`; on Linux it reads `/proc`. If the owner cannot be seen, `who` exits 3 instead of guessing.
