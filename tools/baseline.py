#!/usr/bin/env python3
"""Counts how agents wait for and manage background processes in Claude Code sessions, as the
baseline that devsrv should bring down.

usage: python3 tools/baseline.py [--days 14] [--since 2026-10-07T00:00]

Reads ~/.claude/projects/*/*.jsonl (and subagents) through rpatch's log-analysis session.py
(~/project/prod/rpatch/tools/log-analysis), leaving out prod-rpatch and pi-agent sessions.
Prints counts only, no command text, since sessions can hold credentials.
"""
import argparse
import collections
import os
import re
import sys
import time
from datetime import datetime

sys.path.insert(0, os.path.expanduser("~/project/prod/rpatch/tools/log-analysis"))
import session as s  # noqa: E402

SLEEP = re.compile(r"\bsleep\s+(\d+(?:\.\d+)?)")
LOOP = re.compile(r"\b(for|while|until)\b[^\n]*\bsleep\b|\bsleep\b[^\n]*\bdone\b", re.S)
AFTER_SLEEP = re.compile(r"\bsleep\s+[\d.]+\s*(?:&&|;|\n)\s*(\S+)")
BACKGROUND = re.compile(r"(http\.server|vite|npm run dev|next dev|serve\b|node [^\n|;&]*server[^\n|;&]*|cargo run|uvicorn|flask run)[^\n;|]*&\s*($|\)|;|\n|sleep)")
LSOF_PORT = re.compile(r"lsof\s+-[a-zA-Z]*i|lsof -nP -i")
PORT = re.compile(r":(\d{4,5})\b")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--days", type=float, default=14)
    p.add_argument("--since", help="only calls at or after this UTC time (ISO 8601)")
    a = p.parse_args()
    since = datetime.fromisoformat(a.since).timestamp() if a.since else None
    now = time.time()
    n = collections.Counter()
    after = collections.Counter()
    ports = collections.Counter()
    sleep_secs = 0.0
    for f in s.sessions():
        if now - os.path.getmtime(f) > a.days * 86400:
            break
        if any(x in f for x in s.DEFAULT_EXCLUDE):
            continue
        for t, _ in s.transcripts(f):
            for u in s.load(t)[0]:
                if u["tool"] != "Bash":
                    continue
                if since and u["ts"] and datetime.fromisoformat(u["ts"].replace("Z", "+00:00")).timestamp() < since:
                    continue
                cmd, res = u["input"].get("command") or "", u["res"] or ""
                n["bash"] += 1
                for m in SLEEP.finditer(cmd):
                    n["sleep"] += 1
                    sleep_secs += float(m.group(1))
                n["sleep_loop"] += bool(LOOP.search(cmd))
                n["sleep_blocked_by_claude_code"] += "Blocked: standalone sleep" in res or "Blocked: sleep" in res
                for m in AFTER_SLEEP.finditer(cmd):
                    after[m.group(1)] += 1
                n["background_server_start"] += bool(BACKGROUND.search(cmd))
                n["lsof_port"] += bool(LSOF_PORT.search(cmd))
                n["pkill_f"] += bool(re.search(r"\bpkill\b", cmd))
                n["kill_pid"] += bool(re.search(r"\bkill\b", cmd)) and not re.search(r"\bpkill\b", cmd)
                n["devsrv"] += bool(re.search(r"(^|[;&|(\n]\s*)devsrv\s", cmd))
                n["address_in_use"] += bool(re.search(r"EADDRINUSE|address already in use", res))
                if re.search(r"\b(pkill|kill|lsof|fuser)\b", cmd):
                    ports.update(PORT.findall(cmd))
    print(f"Bash calls: {n['bash']} (last {a.days:g} days{', since ' + a.since if a.since else ''})")
    for key in ("sleep", "sleep_loop", "sleep_blocked_by_claude_code", "background_server_start", "lsof_port",
                "pkill_f", "kill_pid", "address_in_use", "devsrv"):
        print(f"  {key:30} {n[key]}")
    print(f"  {'sleep_minutes':30} {sleep_secs / 60:.0f}")
    print("command right after a sleep:", after.most_common(10))
    print("ports named with kill/lsof:", ports.most_common(10))


if __name__ == "__main__":
    main()
