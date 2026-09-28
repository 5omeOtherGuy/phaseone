#!/usr/bin/env bash
# CI only: make the sandbox tests executable rather than accepting a skipped gate.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y bubblewrap
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
bwrap --unshare-user --ro-bind / / -- true
