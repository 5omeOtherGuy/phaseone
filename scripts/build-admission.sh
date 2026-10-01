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
# P1_MEMINFO (/proc/meminfo), P1_ADMISSION_INTERVAL (10 seconds).
set -uo pipefail
max="${P1_MAX_BUILDS:-3}"
floor_kib="${P1_MEM_FLOOR_KIB:-1258292}"
meminfo="${P1_MEMINFO:-/proc/meminfo}"
interval="${P1_ADMISSION_INTERVAL:-10}"

builds() {
  ps -eo pid=,ppid=,args= | awk '
    {
      parent[$1] = $2
      n = split($3, path, "/")
      if (path[n] == "cargo" && $4 ~ /^(build|test|clippy|check|run|doc|bench|install|rustc)$/) counted[$1] = 1
    }
    END {
      total = 0
      for (pid in counted) {
        nested = 0
        for (up = parent[pid]; up != "" && up != "0" && up != "1" && hops++ < 64; up = parent[up])
          if (up in counted) { nested = 1; break }
        hops = 0
        if (!nested) total++
      }
      print total
    }'
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
