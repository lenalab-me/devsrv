# devsrv verification map

The maintained source for verifying what agents see of devsrv. Read this index, then use the matching feature file as the recipe.

## Baseline preconditions

- `cargo build`, then `scripts/doctor.sh` prints `OK`.
- A fresh session name per run: `S=<feature>-$(date +%s)`. `D=.agents/skills/verify-devsrv/scripts`.
- `python3` on PATH (the recipes use `python3 -m http.server` as the dev server), and `jq` and `curl` (they read the JSON and the address). On Linux, **who-foreign** also needs `lsof`, or use `ss -ltnp` to find the port.

## Driving conventions

- Every call goes through `$D/drive.sh $S …`; observation lines go through `$D/drive.sh $S --sh '…'` with `$DEVSRV`.
- Use `--port auto`. Never use fixed ports: something of the user's may be listening there.
- End every recipe with `$D/cleanup.sh $S`, also after a failed step.

## Proof

- CLI proof is the command, stdout, stderr, the exit code, and `ls-after.txt`.
- Process proof comes from `ps` or `curl`, not from devsrv's own words.

## Features

| File | Covers |
|---|---|
| `start.md` | start, readiness kinds, `{port}`, already started, failures before ready, survives the caller |
| `stop.md` | stop with grandchildren, TERM vs KILL, twice, unknown name, the supervisor gone, a reused pid |
| `wait-and-logs.md` | wait (URL, port, file, gone, log), logs -n, --follow-until |
| `who-and-ls.md` | who for devsrv and foreign listeners, ls text and JSON, two projects |
