#!/usr/bin/env bash
# Stops what a verification session started (devsrv stop --all with the session's
# state dir), and proves nothing of it is left. Keeps the evidence.
# usage: cleanup.sh SESSION
set -euo pipefail

if [ "$#" -ne 1 ]; then
	echo "usage: cleanup.sh SESSION" >&2
	exit 2
fi
repo="$(cd "$(dirname "$0")/../../../.." && pwd)"
bin="${DEVSRV_BIN:-$repo/target/debug/devsrv}"
dir="$repo/target/verify-devsrv/$1"
[ -d "$dir/state" ] || { echo "no session $1"; exit 1; }
cd "$dir/proj"
DEVSRV_STATE_DIR="$dir/state" "$bin" stop --all | tee "$dir/cleanup.txt"
alive=$(DEVSRV_STATE_DIR="$dir/state" "$bin" --json ls | { grep -o '"alive":true' || true; } | wc -l | tr -d ' ')
if [ "$alive" != "0" ]; then
	echo "FAIL: $alive still running in session $1" | tee -a "$dir/cleanup.txt"
	exit 1
fi
echo "clean: nothing of session $1 runs; evidence kept in $dir" | tee -a "$dir/cleanup.txt"
