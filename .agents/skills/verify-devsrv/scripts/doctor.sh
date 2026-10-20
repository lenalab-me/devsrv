#!/usr/bin/env bash
# Read-only: is the binary under test built from the current sources, and is any verify session still running?
set -euo pipefail

repo="$(cd "$(dirname "$0")/../../../.." && pwd)"
bin="${DEVSRV_BIN:-$repo/target/debug/devsrv}"
case "$bin" in /*) ;; *) bin="$PWD/$bin" ;; esac
status=0
if [ ! -x "$bin" ]; then
	echo "FAIL: no binary at $bin (run cargo build)"
	exit 1
fi
want="devsrv $(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)"
have="$("$bin" -V)"
if [ "$have" != "$want" ]; then
	echo "FAIL: $bin is $have, Cargo.toml says $want"
	status=1
fi
if [ -n "$(find "$repo/src" -newer "$bin" -type f | head -1)" ]; then
	echo "FAIL: sources newer than the binary (run cargo build)"
	status=1
fi
for s in "$repo"/target/verify-devsrv/*/; do
	[ -d "$s/state" ] || continue
	if ! listing=$(cd "$s/proj" && DEVSRV_STATE_DIR="$s/state" "$bin" --json ls); then
		echo "NOTE: session $(basename "$s"): devsrv ls failed: $listing"
		continue
	fi
	alive=$(printf '%s' "$listing" | { grep -o '"alive":true' || true; } | wc -l | tr -d ' ')
	if [ "$alive" != "0" ]; then
		echo "NOTE: session $(basename "$s") still has $alive running (cleanup.sh $(basename "$s"))"
	fi
done
echo "PATH devsrv: $(command -v devsrv || echo none)"
[ "$status" = 0 ] && echo OK
exit "$status"
