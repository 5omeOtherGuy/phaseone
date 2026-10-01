#!/usr/bin/env bash
# Waits until a new local Rust build may start (owner 2026-10-01, D25): fewer than three cargo
# builds already running on this machine, and at least 1.2 GiB MemAvailable. While it waits it
# prints why, once per change of reason, then returns 0 when the build may start.
#
# A build is a running `cargo` process whose subcommand compiles (build, test, clippy, check,
# run, doc, bench, install, rustc); a cargo process below another counted cargo process (a test
# binary that runs `cargo build`, say) is the same build. `scripts/rustc-serial` separately
# bounds the rustc processes of all builds.
#
# Settings for tests: P1_MAX_BUILDS (3), P1_MEM_FLOOR_KIB (1258292 = 1.2 GiB),
# P1_MEMINFO (/proc/meminfo), P1_PROC (/proc), P1_ADMISSION_INTERVAL (10 seconds).
set -uo pipefail
max="${P1_MAX_BUILDS:-3}"
floor_kib="${P1_MEM_FLOOR_KIB:-1258292}"
meminfo="${P1_MEMINFO:-/proc/meminfo}"
proc="${P1_PROC:-/proc}"
interval="${P1_ADMISSION_INTERVAL:-10}"
case "$interval" in
  ''|*[!0-9.]*|*.*.*) echo "build-admission: P1_ADMISSION_INTERVAL must be a number of seconds" >&2; exit 2 ;;
esac

# Reads the process table from $proc: each process's parent from `stat` and its argv from
# `cmdline` (NUL-separated, so a path with spaces stays one argument).
builds() {
  python3 - "$proc" <<'PY'
import os, sys
proc = sys.argv[1]
COMPILING = {"build", "test", "clippy", "check", "run", "doc", "bench", "install", "rustc"}
parent, counted = {}, set()
for name in os.listdir(proc):
    if not name.isdigit():
        continue
    try:
        with open(os.path.join(proc, name, "stat"), "rb") as handle:
            stat = handle.read().decode(errors="replace")
        with open(os.path.join(proc, name, "cmdline"), "rb") as handle:
            argv = [a.decode(errors="replace") for a in handle.read().split(b"\0") if a]
    except OSError:
        continue  # the process ended while the table was read
    # `pid (comm) state ppid ...`: comm may hold spaces and parentheses, so split after the last ')'.
    fields = stat[stat.rfind(")") + 2:].split()
    if len(fields) < 2:
        continue
    parent[name] = fields[1]
    if argv and os.path.basename(argv[0]) == "cargo":
        rest = argv[1:]
        if rest and rest[0].startswith("+"):  # `cargo +toolchain test`
            rest = rest[1:]
        if rest and rest[0] in COMPILING:
            counted.add(name)
total = 0
for pid in counted:
    seen, up, nested = {pid}, parent.get(pid), False
    while up and up not in seen:  # every ancestor, however deep; a cycle ends the walk
        if up in counted:
            nested = True
            break
        seen.add(up)
        up = parent.get(up)
    total += not nested
print(total)
PY
}

said=""
while :; do
  running="$(builds)" || running=0
  avail="$(awk '/^MemAvailable:/ { print $2 }' "$meminfo" 2>/dev/null)"
  avail="${avail:-0}"
  reason=""
  if [ "$running" -ge "$max" ]; then
    reason="$running cargo builds running (at most $max)"
  fi
  if [ "$avail" -lt "$floor_kib" ]; then
    reason="${reason:+$reason; }MemAvailable $((avail / 1024)) MiB (floor $((floor_kib / 1024)) MiB)"
  fi
  [ -n "$reason" ] || exit 0
  if [ "$reason" != "$said" ]; then
    echo "build-admission: waiting: $reason" >&2
    said="$reason"
  fi
  sleep "$interval"
done
