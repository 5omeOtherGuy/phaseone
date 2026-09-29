#!/usr/bin/env bash
# CI only: make the sandbox tests executable rather than accepting a skipped gate.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y bubblewrap
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
# Exactly the probe scripts/gate.sh and the Rust boundary suites run, so passing here means
# the gate's probe and the suites will not skip on this runner (Codex finding build.yml:92).
bwrap --ro-bind / / --dev /dev --proc /proc true
