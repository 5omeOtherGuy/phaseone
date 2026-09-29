#!/usr/bin/env bash
# CI only: make the sandbox tests executable rather than accepting a skipped gate.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y bubblewrap
# GitHub's Ubuntu 24.04 image restricts unprivileged user namespaces through AppArmor; Depot
# CI's runner kernel has no such knob, so there is nothing to lift there.
if [ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi
# Exactly the probe scripts/gate.sh and the Rust boundary suites run, so passing here means
# the gate's probe and the suites will not skip on this runner (Codex finding build.yml:92).
bwrap --ro-bind / / --dev /dev --proc /proc true
