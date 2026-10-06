#!/usr/bin/env bash
# Runs one devsrv command inside a verification session and keeps the evidence.
# usage: drive.sh SESSION [devsrv args...]
#        drive.sh SESSION --sh 'shell line'   (runs a line in the session's project dir,
#                                              with $DEVSRV pointing at the binary)
set -euo pipefail

if [ "$#" -lt 2 ]; then
	echo "usage: drive.sh SESSION devsrv-args... | drive.sh SESSION --sh 'line'" >&2
	exit 2
fi
repo="$(cd "$(dirname "$0")/../../../.." && pwd)"
bin="${DEVSRV_BIN:-$repo/target/debug/devsrv}"
session="$1"
shift
dir="$repo/target/verify-devsrv/$session"
mkdir -p "$dir/proj/.git" "$dir/state"
n=$(find "$dir" -maxdepth 1 -name 'step-*' | wc -l | tr -d ' ')
step="$dir/step-$(printf '%02d' $((n + 1)))"
mkdir -p "$step"
printf '%s\n' "$*" > "$step/args.txt"

set +e
if [ "$1" = "--sh" ]; then
	(cd "$dir/proj" && DEVSRV="$bin" DEVSRV_STATE_DIR="$dir/state" sh -c "$2") > "$step/stdout.txt" 2> "$step/stderr.txt"
else
	(cd "$dir/proj" && DEVSRV_STATE_DIR="$dir/state" "$bin" "$@") > "$step/stdout.txt" 2> "$step/stderr.txt"
fi
echo $? > "$step/exit_code"
(cd "$dir/proj" && DEVSRV_STATE_DIR="$dir/state" "$bin" ls) > "$step/ls-after.txt" 2>&1
set -e

{
	echo "session:   $session ($dir)"
	echo "step:      $(basename "$step")"
	echo "args:      $*"
	echo "exit code: $(cat "$step/exit_code")"
	echo "--- stdout"
	cat "$step/stdout.txt"
	echo "--- stderr"
	cat "$step/stderr.txt"
	echo "--- devsrv ls after"
	cat "$step/ls-after.txt"
} | tee "$step/summary.txt"
